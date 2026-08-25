//! Loopback integration tests for the discovery TCP scan and orchestrator.
//!
//! Scan tests use zero ffmpeg: only std TcpListener sockets on 127.0.0.1.
//! The pipeline smoke test self-skips without ffprobe (mirrors
//! tests/ffmpeg_pipe.rs).

use cam_viewer::discover::scan::{self, Responder};
use cam_viewer::discover::{DiscoveryConfig, DiscoveryHandle, Phase};
use std::net::{Ipv4Addr, TcpListener};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::thread;
use std::time::{Duration, Instant};

/// Binds an ephemeral loopback listener; the caller must keep it alive for
/// the duration of the scan under test.
fn open_loopback_listener() -> TcpListener {
    TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral loopback listener")
}

fn port_of(listener: &TcpListener) -> u16 {
    listener.local_addr().expect("local addr").port()
}

#[test]
fn scan_reports_listener_as_responder() {
    let listener = open_loopback_listener();
    let port = port_of(&listener);
    let cancel = AtomicBool::new(false);
    let progress = AtomicU32::new(0);

    let responders = scan::scan(&[Ipv4Addr::LOCALHOST], &[port], &cancel, &progress);

    assert_eq!(
        responders,
        vec![Responder {
            ip: Ipv4Addr::LOCALHOST,
            port,
        }],
        "loopback listener must be reported as a responder on its port"
    );
}

#[test]
fn silent_non_responder_yields_no_entry_and_no_error() {
    let closed = {
        let listener = open_loopback_listener();
        port_of(&listener)
    };
    let cancel = AtomicBool::new(false);
    let progress = AtomicU32::new(0);

    let responders = scan::scan(&[Ipv4Addr::LOCALHOST], &[closed], &cancel, &progress);

    assert!(
        responders.is_empty(),
        "closed loopback port is a silent non-responder: {responders:?}"
    );
}

#[test]
fn preset_cancel_returns_promptly_with_no_results() {
    let listener = open_loopback_listener();
    let port = port_of(&listener);
    let cancel = AtomicBool::new(true);
    let progress = AtomicU32::new(0);

    let started = Instant::now();
    let responders = scan::scan(&[Ipv4Addr::LOCALHOST], &[port], &cancel, &progress);
    let elapsed = started.elapsed();

    assert!(
        responders.is_empty(),
        "pre-set cancel must yield no results"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "scan with pre-set cancel must return promptly, took {elapsed:?}"
    );
}

#[test]
fn progress_counter_is_monotonic_and_reaches_total_work_units() {
    let listener = open_loopback_listener();
    let open_port = port_of(&listener);
    let ports = [open_port, 9, 1];
    let cancel = AtomicBool::new(false);
    let progress = AtomicU32::new(0);

    let _ = scan::scan(&[Ipv4Addr::LOCALHOST], &ports, &cancel, &progress);

    assert_eq!(
        progress.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "one work unit per (host, port) pair must be counted"
    );
}

fn is_terminal(phase: &Phase) -> bool {
    matches!(phase, Phase::Complete | Phase::Cancelled | Phase::Failed(_))
}

fn test_net_1_subnet() -> cam_viewer::discover::net::Subnet {
    cam_viewer::discover::net::Subnet {
        network: u32::from(Ipv4Addr::new(192, 0, 2, 0)),
        prefix: 24,
    }
}

fn loopback_subnet() -> cam_viewer::discover::net::Subnet {
    cam_viewer::discover::net::Subnet {
        network: u32::from(Ipv4Addr::new(127, 0, 0, 0)),
        prefix: 24,
    }
}

#[test]
fn cancel_reaches_terminal_phase_within_two_seconds() {
    use cam_viewer::discover::probe;

    let handle = DiscoveryHandle::start(DiscoveryConfig {
        subnet: test_net_1_subnet(),
        ports: scan::DEFAULT_PORTS.to_vec(),
        creds: None,
        probe_timeout: probe::DISCOVER_PROBE_TIMEOUT,
    });

    thread::sleep(Duration::from_millis(200));
    handle.cancel();

    let started = Instant::now();
    loop {
        let phase = handle.snapshot().phase;
        if is_terminal(&phase) {
            assert!(
                matches!(phase, Phase::Cancelled),
                "cancel must land on Cancelled, got {phase:?}"
            );
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "terminal phase not reached in time"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn pipeline_smoke_on_loopback_completes_with_monotonic_counters() {
    let ok_ffprobe = Command::new("ffprobe")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !ok_ffprobe {
        eprintln!("ffprobe not available, skipping pipeline smoke");
        return;
    }

    let listener = open_loopback_listener();
    let port = port_of(&listener);

    let handle = DiscoveryHandle::start(DiscoveryConfig {
        subnet: loopback_subnet(),
        ports: vec![port],
        creds: None,
        probe_timeout: Duration::from_millis(500),
    });

    let mut phases_seen = Vec::new();
    let mut previous = (0u32, 0u32, 0u32);
    let started = Instant::now();
    loop {
        let snap = handle.snapshot();
        if !phases_seen.contains(&snap.phase) {
            phases_seen.push(snap.phase.clone());
        }
        let current = (snap.hosts_scanned, snap.responders_found, snap.probes_done);
        assert!(
            current.0 >= previous.0 && current.1 >= previous.1 && current.2 >= previous.2,
            "counters must advance monotonically: {previous:?} -> {current:?}"
        );
        previous = current;
        if is_terminal(&snap.phase) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "pipeline did not reach a terminal phase in time"
        );
        thread::sleep(Duration::from_millis(25));
    }

    let final_snap = handle.snapshot();
    assert_eq!(
        final_snap.phase,
        Phase::Complete,
        "gated smoke with a live listener must complete; phases {phases_seen:?}"
    );
    assert!(
        phases_seen.iter().any(|p| matches!(p, Phase::Scanning)),
        "Scanning phase must be observed; saw {phases_seen:?}"
    );
    assert_eq!(final_snap.hosts_scanned, 254);
    assert!(
        final_snap.responders_found >= 1,
        "loopback listener must be found as a responder"
    );
}
