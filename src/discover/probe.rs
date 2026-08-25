//! Vendor-path candidate generation, per-host probing policy, and the
//! process-backed probe pool (spec REQ-6, REQ-7, REQ-9, REQ-10).

use crate::discover::scan::Responder;
use crate::discover::{AuthStatus, DiscoveryResult};
use crate::stream::{self, ProbeOutcome};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Hard cap on ffprobe attempts against a single host, regardless of how
/// large the paths x ports matrix grows (Reconciliation 3; lockout safety).
pub const MAX_ATTEMPTS_PER_HOST: usize = 8;

/// Worker count for the probe pool; bounds concurrent ffprobe processes.
pub const PROBE_WORKERS: usize = 16;

/// Per-attempt deadline handed to [`stream::probe_rtsp`].
pub const DISCOVER_PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// Data-driven vendor path table: `(vendor label, RTSP path)`. New vendors
/// are one table row; candidate generation needs no logic change (REQ-7).
pub static VENDOR_PATHS: &[(&str, &str)] = &[
    ("Hikvision", "/Streaming/Channels/101"),
    ("Dahua/Amcrest", "/cam/realmonitor?channel=1&subtype=0"),
    ("TP-Link", "/stream1"),
    ("Reolink", "/h264Preview_01_main"),
    ("Reolink ONVIF", "/onvif1"),
    ("Axis", "/axis-media/media.amp"),
    ("Foscam", "/videoMain"),
    ("Generic", "/live/ch0"),
    ("Root", "/"),
];

/// Deterministic union of [`VENDOR_PATHS`] rendered against one host port as
/// `rtsp://[user:pass@]ip:port<path>`. Credentials are inserted verbatim —
/// byte-exact, never percent-encoded — for parity with manual entry (REQ-6).
pub fn candidate_urls(ip: Ipv4Addr, port: u16, creds: Option<(&str, &str)>) -> Vec<String> {
    let userinfo = match creds {
        Some((user, password)) => format!("{user}:{password}@"),
        None => String::new(),
    };
    VENDOR_PATHS
        .iter()
        .map(|(_, path)| format!("rtsp://{userinfo}{ip}:{port}{path}"))
        .collect()
}

/// Per-host stop-rule policy (spec REQ-9), generic over the actual attempt
/// mechanism so tests inject a closure instead of spawning ffprobe.
///
/// - `Success` ends the host immediately (stop-on-first-success).
/// - `BadCredentials` proves the port speaks RTSP: remaining paths on that
///   port are skipped (fewer lockout hits), probing moves to the next port.
/// - `PortClosed`/`Unreachable`/`NotRtsp` stop the port.
/// - `PathNotFound` means the server is live but the path is wrong: try the
///   next path on the same port.
/// - At most `max_attempts` closure invocations happen per host.
///
/// Returns a result row only when the host earned one per REQ-10: a Success
/// (Open or Authenticated) or a BadCredentials-only best outcome
/// (`NeedsCredentials`). Transport/service-failure hosts yield `None`.
fn probe_host(
    ip: Ipv4Addr,
    ports: &[u16],
    creds: Option<(&str, &str)>,
    max_attempts: usize,
    attempt: &mut dyn FnMut(&str) -> ProbeOutcome,
) -> Option<DiscoveryResult> {
    let mut attempts = 0usize;
    let mut saw_bad_credentials = false;
    'host: for port in ports {
        let urls = candidate_urls(ip, *port, creds);
        for (url, (vendor, _)) in urls.iter().zip(VENDOR_PATHS) {
            if attempts >= max_attempts {
                break 'host;
            }
            attempts += 1;
            match attempt(url) {
                ProbeOutcome::Success { width, height } => {
                    let auth = if creds.is_some() {
                        AuthStatus::Authenticated
                    } else {
                        AuthStatus::Open
                    };
                    return Some(DiscoveryResult {
                        ip,
                        url: Some(url.clone()),
                        vendor: Some((*vendor).to_owned()),
                        resolution: Some((width, height)),
                        auth,
                    });
                }
                ProbeOutcome::BadCredentials => {
                    saw_bad_credentials = true;
                    continue 'host;
                }
                ProbeOutcome::PathNotFound => {}
                ProbeOutcome::PortClosed | ProbeOutcome::Unreachable | ProbeOutcome::NotRtsp => {
                    continue 'host;
                }
            }
        }
    }
    if saw_bad_credentials {
        return Some(DiscoveryResult {
            ip,
            url: None,
            vendor: None,
            resolution: None,
            auth: AuthStatus::NeedsCredentials,
        });
    }
    None
}

/// Whether the ffprobe binary can actually be spawned. Checked once per pool
/// run so a missing binary surfaces as the fatal `ffprobe not found` condition
/// (consumed by the orchestrator as `Phase::Failed`) instead of being mistaken
/// for per-host Unreachable outcomes — scan results survive that path.
pub fn ffprobe_available() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Probes every responder host on up to [`PROBE_WORKERS`] scoped threads.
/// Distribution is HOST-AFFINE (each host is handled start-to-finish by one
/// worker), so at most one probe attempt ever runs against a camera at a
/// time. Every attempt calls [`stream::probe_rtsp`] with `timeout`; rows for
/// hosts that earned one are pushed into `sink`.
///
/// Returns `false` when ffprobe is not available and nothing was probed
/// (deviation from the design's unit return type, required by the batch-1
/// gate note so the orchestrator can raise `Phase::Failed("ffprobe not
/// found")` with scan results preserved). `progress` counts spawned attempts.
pub fn probe_pool(
    responders: Vec<Responder>,
    creds: Option<(String, String)>,
    cancel: &AtomicBool,
    progress: &AtomicU32,
    sink: &Mutex<Vec<DiscoveryResult>>,
    timeout: Duration,
) -> bool {
    if !ffprobe_available() {
        return false;
    }

    let mut by_ip: BTreeMap<Ipv4Addr, Vec<u16>> = BTreeMap::new();
    for responder in responders {
        by_ip.entry(responder.ip).or_default().push(responder.port);
    }
    let ports_per_ip = by_ip.len();

    let (sender, receiver) = mpsc::channel::<(Ipv4Addr, Vec<u16>)>();
    for host in by_ip {
        if sender.send(host).is_err() {
            break;
        }
    }
    drop(sender);

    let receiver = Mutex::new(receiver);
    thread::scope(|scope| {
        let receiver = &receiver;
        let creds = creds
            .as_ref()
            .map(|(user, pass)| (user.as_str(), pass.as_str()));
        let sink = &sink;
        for _ in 0..PROBE_WORKERS.min(ports_per_ip.max(1)) {
            scope.spawn(move || {
                loop {
                    let task = {
                        let queue = lock(receiver);
                        queue.recv()
                    };
                    let Ok((ip, ports)) = task else {
                        break;
                    };
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut attempt = |url: &str| -> ProbeOutcome {
                        if cancel.load(Ordering::Relaxed) {
                            return ProbeOutcome::Unreachable;
                        }
                        progress.fetch_add(1, Ordering::Relaxed);
                        stream::probe_rtsp(url, timeout)
                    };
                    if let Some(result) =
                        probe_host(ip, &ports, creds, MAX_ATTEMPTS_PER_HOST, &mut attempt)
                        && !cancel.load(Ordering::Relaxed)
                    {
                        lock(sink).push(result);
                    }
                }
            });
        }
    });
    true
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{VENDOR_PATHS, candidate_urls};
    use std::net::Ipv4Addr;

    const IP: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 64);

    #[test]
    fn candidates_render_every_table_path_once_with_stable_order() {
        let urls = candidate_urls(IP, 554, None);
        let expected: Vec<String> = VENDOR_PATHS
            .iter()
            .map(|(_, path)| format!("rtsp://{IP}:554{path}"))
            .collect();
        assert_eq!(
            urls, expected,
            "candidate set must be computed from the table"
        );
        let unique = urls.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(
            unique.len(),
            urls.len(),
            "each path must appear exactly once"
        );
    }

    #[test]
    fn credentials_embedded_verbatim_without_percent_encoding() {
        let urls = candidate_urls(IP, 554, Some(("admin", "pa ss!")));
        let expected: Vec<String> = VENDOR_PATHS
            .iter()
            .map(|(_, path)| format!("rtsp://admin:pa ss!@{IP}:554{path}"))
            .collect();
        assert_eq!(
            urls, expected,
            "credential bytes must be inserted byte-exact, like manual entry"
        );
        assert!(
            !urls[0].contains('%'),
            "no percent-encoding may be applied to credentials"
        );
    }

    #[test]
    fn empty_credentials_produce_bare_urls() {
        let urls = candidate_urls(IP, 8554, None);
        assert!(
            urls.first()
                .expect("non-empty")
                .starts_with(&format!("rtsp://{IP}:8554/"))
        );
        assert!(
            !urls[0].contains('@'),
            "anonymous candidates must carry no userinfo"
        );
    }

    #[test]
    fn appending_a_table_row_extends_candidates_with_zero_logic_change() {
        let baseline_len = candidate_urls(IP, 554, None).len();
        assert_eq!(
            baseline_len,
            VENDOR_PATHS.len(),
            "one candidate per table row"
        );
        for (_, path) in VENDOR_PATHS {
            let matching = candidate_urls(IP, 554, None)
                .into_iter()
                .filter(|url| url.ends_with(path))
                .count();
            assert_eq!(matching, 1, "path {path} must render exactly once");
        }
    }
}

#[cfg(test)]
mod policy_tests {
    use super::{MAX_ATTEMPTS_PER_HOST, VENDOR_PATHS, candidate_urls, probe_host};
    use crate::discover::AuthStatus;
    use crate::stream::ProbeOutcome;
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const IP: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 64);
    const PORT_A: u16 = 554;
    const PORT_B: u16 = 8554;

    /// Outcome sequence served by the fake attempt closure, in spawn order.
    fn scripted(outcomes: Vec<ProbeOutcome>) -> impl FnMut(&str) -> ProbeOutcome {
        let mut outcomes = outcomes;
        move |_: &str| outcomes.remove(0)
    }

    #[test]
    fn second_candidate_success_stops_host_after_two_attempts() {
        let attempts = AtomicUsize::new(0);
        let mut attempt = |_url: &str| {
            let n = attempts.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                ProbeOutcome::PathNotFound
            } else {
                ProbeOutcome::Success {
                    width: 1920,
                    height: 1080,
                }
            }
        };

        let result = probe_host(IP, &[PORT_A], None, usize::MAX, &mut attempt);

        let result = result.expect("successful host must produce a row");
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            2,
            "third candidate never spawned"
        );
        let expected_url = &candidate_urls(IP, PORT_A, None)[1];
        assert_eq!(result.url.as_deref(), Some(expected_url.as_str()));
        assert_eq!(result.auth, AuthStatus::Open, "anonymous success");
        assert_eq!(result.resolution, Some((1920, 1080)));
        assert_eq!(result.vendor.as_deref(), Some(VENDOR_PATHS[1].0));
    }

    #[test]
    fn credentialed_success_maps_to_authenticated() {
        let outcomes = vec![ProbeOutcome::Success {
            width: 1280,
            height: 720,
        }];
        let mut attempt = scripted(outcomes);

        let result = probe_host(
            IP,
            &[PORT_A],
            Some(("admin", "s3cret")),
            usize::MAX,
            &mut attempt,
        );

        let result = result.expect("row");
        assert_eq!(result.auth, AuthStatus::Authenticated);
        assert!(result.url.expect("url").contains("admin:s3cret@"));
    }

    #[test]
    fn all_bad_credentials_hits_each_port_once_and_keeps_warning_row() {
        let attempts = AtomicUsize::new(0);
        let mut attempt = |_: &str| {
            attempts.fetch_add(1, Ordering::Relaxed);
            ProbeOutcome::BadCredentials
        };

        let result = probe_host(IP, &[PORT_A, PORT_B], None, 100, &mut attempt);

        let result = result.expect("bad-credentials host keeps a warning row");
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            2,
            "one auth hit per port: remaining paths are skipped (lockout safety)"
        );
        assert_eq!(result.auth, AuthStatus::NeedsCredentials);
        assert!(
            result.url.is_none(),
            "no addable working URL without success"
        );
    }

    #[test]
    fn every_path_not_found_walks_the_full_matrix_exactly() {
        let attempts = AtomicUsize::new(0);
        let mut attempt = |_: &str| {
            attempts.fetch_add(1, Ordering::Relaxed);
            ProbeOutcome::PathNotFound
        };

        let result = probe_host(IP, &[PORT_A, PORT_B], None, 100, &mut attempt);

        assert!(result.is_none(), "path-not-found-only hosts render no rows");
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            VENDOR_PATHS.len() * 2,
            "wrong path keeps trying candidates: full P x T matrix walked"
        );
    }

    #[test]
    fn hard_cap_limits_attempts_regardless_of_matrix_size() {
        let attempts = AtomicUsize::new(0);
        let mut attempt = |_: &str| {
            attempts.fetch_add(1, Ordering::Relaxed);
            ProbeOutcome::PathNotFound
        };

        let _ = probe_host(
            IP,
            &[PORT_A, PORT_B],
            None,
            MAX_ATTEMPTS_PER_HOST,
            &mut attempt,
        );

        assert_eq!(
            attempts.load(Ordering::Relaxed),
            MAX_ATTEMPTS_PER_HOST,
            "cap enforced even though matrix is larger"
        );
    }

    #[test]
    fn bad_credentials_skips_remaining_paths_on_port_but_tries_next_port() {
        let attempts = AtomicUsize::new(0);
        let mut attempt = |url: &str| {
            let n = attempts.fetch_add(1, Ordering::Relaxed);
            if url.contains(&format!(":{PORT_A}")) {
                ProbeOutcome::BadCredentials
            } else {
                assert!(url.contains(&format!(":{PORT_B}")));
                let _ = n;
                ProbeOutcome::Success {
                    width: 640,
                    height: 480,
                }
            }
        };

        let result = probe_host(IP, &[PORT_A, PORT_B], None, usize::MAX, &mut attempt);

        let result = result.expect("row");
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            2,
            "one auth hit proves port A speaks RTSP; only port B's first path runs"
        );
        assert!(result.url.expect("url").contains(&format!(":{PORT_B}")));
    }

    #[test]
    fn transport_failure_stops_port_and_falls_through_to_next_port() {
        let mut attempt = |url: &str| {
            if url.contains(&format!(":{PORT_A}")) {
                ProbeOutcome::Unreachable
            } else {
                ProbeOutcome::Success {
                    width: 640,
                    height: 480,
                }
            }
        };

        let result = probe_host(IP, &[PORT_A, PORT_B], None, usize::MAX, &mut attempt);

        let result = result.expect("row");
        assert!(result.url.expect("url").contains(&format!(":{PORT_B}")));
    }

    #[test]
    fn transport_only_host_produces_no_row() {
        let mut attempt = |_: &str| ProbeOutcome::PortClosed;

        let result = probe_host(IP, &[PORT_A], None, usize::MAX, &mut attempt);

        assert!(
            result.is_none(),
            "transport/service failures never render rows"
        );
    }
}
