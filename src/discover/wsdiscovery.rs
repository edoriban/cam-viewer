use crate::discover::net;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Multicast group and port for WS-Discovery (ONVIF device discovery).
const DISCOVERY_TARGET: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)), 3702);

/// Default listen window; keeps the overall pipeline budget intact.
pub const PROBE_WAIT: Duration = Duration::from_millis(1500);

/// Per-`recv_from` tick so cancellation is observed promptly.
const READ_TICK: Duration = Duration::from_millis(250);

/// Result of the secondary source. `degraded` distinguishes "socket-level
/// failure" from "no replies"; both carry an empty list and never panic
/// upward — the TCP-only path continues unaffected either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsDiscoveryOutcome {
    pub addresses: Vec<Ipv4Addr>,
    pub degraded: bool,
}

/// Sends one SOAP Probe to [`DISCOVERY_TARGET`] and listens for
/// [`PROBE_WAIT`], scraping XAddrs from replies. ANY socket-level error
/// (bind, timeout setup, send) degrades totally: empty list + `degraded`,
/// no panic. `cancel` breaks the receive loop between ticks.
pub fn discover(probe_wait: Duration, cancel: &AtomicBool) -> WsDiscoveryOutcome {
    let degrade = || WsDiscoveryOutcome {
        addresses: Vec::new(),
        degraded: true,
    };
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return degrade();
    };
    if socket.set_read_timeout(Some(READ_TICK)).is_err() {
        return degrade();
    }
    let mut own_ips: Vec<Ipv4Addr> = net::list_interfaces().into_iter().map(|i| i.ip).collect();
    own_ips.push(Ipv4Addr::LOCALHOST);

    let probe = build_probe_xml(&message_id(&socket));
    if socket.send_to(probe.as_bytes(), DISCOVERY_TARGET).is_err() {
        return degrade();
    }

    let deadline = Instant::now() + probe_wait;
    let mut addresses = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    while Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if let Ok((read, _from)) = socket.recv_from(&mut buffer) {
            let reply = String::from_utf8_lossy(&buffer[..read]);
            addresses.extend(scrape_xaddrs(&reply, &own_ips));
        } // tick timeout or transient receive error: keep waiting
    }
    WsDiscoveryOutcome {
        addresses,
        degraded: false,
    }
}

fn build_probe_xml(message_id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
<e:Header>
<w:MessageID>uuid:{message_id}</w:MessageID>
<w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>
<w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action>
</e:Header>
<e:Body>
<d:Probe>
<d:Types>dn:NetworkVideoTransmitter</d:Types>
</d:Probe>
</e:Body>
</e:Envelope>"#
    )
}

/// Extracts IPv4 authorities from every (optionally prefixed,
/// case-insensitive) `XAddrs` element: the text between the element's `>` and
/// its closing `<` is split on whitespace, each URL's authority is taken
/// between `://` and the next `:` or `/`, and only parseable addresses are
/// kept. Hostnames are dropped; duplicates and own IPs are removed.
fn scrape_xaddrs(text: &str, own_ips: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    let lower = text.to_lowercase();
    let mut found = Vec::new();
    let mut cursor = 0usize;
    while let Some(tag_pos) = lower[cursor..].find("xaddrs") {
        let tag_abs = cursor + tag_pos;
        let Some(content_start_rel) = lower[tag_abs..].find('>') else {
            break;
        };
        let content_start = tag_abs + content_start_rel + 1;
        let Some(content_end_rel) = lower[content_start..].find('<') else {
            break;
        };
        let content_end = content_start + content_end_rel;
        for url in text[content_start..content_end].split_whitespace() {
            if let Some(ip) = authority_ipv4(url)
                && !own_ips.contains(&ip)
                && !found.contains(&ip)
            {
                found.push(ip);
            }
        }
        cursor = content_end;
    }
    found
}

fn authority_ipv4(url: &str) -> Option<Ipv4Addr> {
    let after_scheme = url.split_once("://")?.1;
    let authority_end = after_scheme.find([':', '/']).unwrap_or(after_scheme.len());
    after_scheme[..authority_end].parse().ok()
}

/// uuid-shaped MessageID from system-time nanos mixed with the socket's
/// ephemeral port — no uuid crate (relates-to matching is deliberately
/// skipped per design).
fn message_id(socket: &UdpSocket) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let port = socket.local_addr().map_or(0, |a| u64::from(a.port()));
    let value = nanos.rotate_left(17) ^ port;
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        value >> 32,
        (value >> 16) & 0xffff,
        (value >> 12) & 0xfff,
        value & 0xfff,
        port.rotate_left(16) & 0xffff_ffff_ffff,
    )
}

#[cfg(test)]
mod tests {
    use super::{WsDiscoveryOutcome, build_probe_xml, discover, scrape_xaddrs};
    use std::net::Ipv4Addr;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    #[test]
    fn probe_xml_contains_probe_element_type_action_and_message_id() {
        let xml = build_probe_xml("deadbeef-1234");
        assert!(
            xml.contains("Probe"),
            "SOAP body must carry a Probe element"
        );
        assert!(
            xml.contains("dn:NetworkVideoTransmitter"),
            "device type must target network video transmitters"
        );
        assert!(
            xml.contains("http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe"),
            "standard discovery Action URI required"
        );
        assert!(
            xml.contains("uuid:deadbeef-1234"),
            "given MessageID embedded"
        );
    }

    #[test]
    fn scrape_handles_prefixed_tags() {
        let text = r#"<d:XAddrs>http://192.168.1.77/onvif/device_service</d:XAddrs>"#;
        assert_eq!(
            scrape_xaddrs(text, &[]),
            vec![Ipv4Addr::new(192, 168, 1, 77)]
        );
    }

    #[test]
    fn scrape_handles_unprefixed_and_case_insensitive_tags() {
        let text = r#"<xaddrs>http://10.0.0.23:8899/onvif/</xaddrs>"#;
        assert_eq!(scrape_xaddrs(text, &[]), vec![Ipv4Addr::new(10, 0, 0, 23)]);
    }

    #[test]
    fn scrape_extracts_authority_stopping_at_port_or_path() {
        let text = "<XAddrs>http://192.168.1.77:80/onvif/ http://192.168.1.78/onvif/device_service</XAddrs>";
        assert_eq!(
            scrape_xaddrs(text, &[]),
            vec![
                Ipv4Addr::new(192, 168, 1, 77),
                Ipv4Addr::new(192, 168, 1, 78),
            ]
        );
    }

    #[test]
    fn scrape_drops_hostnames_and_unparseable_authorities() {
        let text =
            "<XAddrs>http://cam.local/onvif/ urn:x-addrs:garbage http://999.1.1.1/x</XAddrs>";
        assert!(
            scrape_xaddrs(text, &[]).is_empty(),
            "only parseable Ipv4Addr kept"
        );
    }

    #[test]
    fn scrape_dedups_and_drops_own_addresses() {
        let own = [Ipv4Addr::new(192, 168, 1, 5)];
        let text = "<XAddrs>http://192.168.1.5/onvif/ http://192.168.1.5:8080/onvif/ http://192.168.1.6/onvif/</XAddrs>";
        assert_eq!(
            scrape_xaddrs(text, &own),
            vec![Ipv4Addr::new(192, 168, 1, 6)],
            "own IPs dropped, duplicates collapsed"
        );
    }

    #[test]
    fn scrape_finds_multiple_xaddrs_elements_in_one_reply() {
        let text =
            "<a><XAddrs>http://10.1.1.9/x</XAddrs></a><b><XAddrs>http://10.1.1.10/y</XAddrs></b>";
        assert_eq!(
            scrape_xaddrs(text, &[]),
            vec![Ipv4Addr::new(10, 1, 1, 9), Ipv4Addr::new(10, 1, 1, 10)]
        );
    }

    #[test]
    fn discover_with_zero_responders_completes_within_two_seconds() {
        let cancel = AtomicBool::new(false);
        let started = Instant::now();

        let WsDiscoveryOutcome { .. } = discover(Duration::from_millis(1500), &cancel);

        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(2000),
            "WS-Discovery wait must be strictly time-bounded, took {elapsed:?}"
        );
    }
}
