//! Background LAN camera auto-discovery: interface enumeration, TCP scan,
//! ffprobe probing, and WS-Discovery augmentation, orchestrated on one
//! background thread and observed by the UI through cheap snapshots.

pub mod net;
pub mod probe;
pub mod scan;
pub mod wsdiscovery;

use crate::discover::scan::Responder;
use net::Subnet;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

/// Poison-tolerant lock so a panicked worker never wedges the UI.
/// Mirrors the local helper in stream.rs by design decision (duplicated,
/// not exported).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Authentication outcome for a discovered host.
///
/// `NeedsCredentials` is warning-row scaffolding: per spec REQ-10 such hosts
/// render as rows with a warning dot and a DISABLED checkbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    /// Streams anonymously.
    Open,
    /// Succeeded with the global credential pair.
    Authenticated,
    /// Best outcome was BadCredentials-only; not addable until credentials
    /// are supplied and a rescan succeeds.
    NeedsCredentials,
}

/// One discovered host: best outcome, working URL (lowest-port success
/// preferred), vendor guess from the winning table row, resolution, and
/// auth status. Hosts whose best outcome is PortClosed/Unreachable/NotRtsp
/// never produce one.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryResult {
    pub ip: Ipv4Addr,
    /// Working URL with credentials embedded verbatim; None unless probing
    /// achieved Success.
    pub url: Option<String>,
    pub vendor: Option<String>,
    pub resolution: Option<(usize, usize)>,
    pub auth: AuthStatus,
}

/// Lifecycle phases of a discovery run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Configuring,
    Scanning,
    Complete,
    Cancelled,
    Failed(String),
}

/// Shared mutable discovery state owned by the orchestrator thread.
#[derive(Debug)]
pub struct DiscoveryState {
    pub phase: Phase,
    pub hosts_scanned: u32,
    pub hosts_total: u32,
    pub responders_found: u32,
    pub probes_done: u32,
    pub probes_total: u32,
    pub results: Vec<DiscoveryResult>,
    pub error: Option<String>,
    /// WS-Discovery secondary source bookkeeping (REQ-11 hint).
    pub ws_found: u32,
    pub ws_degraded: bool,
}

impl Default for DiscoveryState {
    fn default() -> Self {
        Self {
            phase: Phase::Configuring,
            hosts_scanned: 0,
            hosts_total: 0,
            responders_found: 0,
            probes_done: 0,
            probes_total: 0,
            results: Vec::new(),
            error: None,
            ws_found: 0,
            ws_degraded: false,
        }
    }
}

/// Cheap UI-facing clone of [`DiscoveryState`]; pulled once per repaint.
#[derive(Clone, Debug)]
pub struct DiscoverySnapshot {
    pub phase: Phase,
    pub hosts_scanned: u32,
    pub hosts_total: u32,
    pub responders_found: u32,
    pub probes_done: u32,
    pub probes_total: u32,
    pub results: Vec<DiscoveryResult>,
    pub ws_found: u32,
    pub ws_degraded: bool,
}

impl From<&DiscoveryState> for DiscoverySnapshot {
    fn from(state: &DiscoveryState) -> Self {
        Self {
            phase: state.phase.clone(),
            hosts_scanned: state.hosts_scanned,
            hosts_total: state.hosts_total,
            responders_found: state.responders_found,
            probes_done: state.probes_done,
            probes_total: state.probes_total,
            results: state.results.clone(),
            ws_found: state.ws_found,
            ws_degraded: state.ws_degraded,
        }
    }
}

/// Everything one discovery run needs. Owned values so the handle stays
/// `'static`; production callers pass [`scan::DEFAULT_PORTS`] and
/// [`probe::DISCOVER_PROBE_TIMEOUT`] explicitly.
#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    pub subnet: Subnet,
    pub ports: Vec<u16>,
    pub creds: Option<(String, String)>,
    pub probe_timeout: Duration,
}

/// Cancellable handle over a running discovery pipeline; dropping it cancels
/// and joins every worker thread (no leaked threads or ffprobe children).
pub struct DiscoveryHandle {
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<DiscoveryState>>,
    join: Option<thread::JoinHandle<()>>,
}

impl DiscoveryHandle {
    /// Spawns the orchestrator thread running the sequential pipeline:
    /// wsdiscovery -> merge IPs dedup -> scan -> consolidate -> probe_pool.
    pub fn start(config: DiscoveryConfig) -> Self {
        let shared = HandleShared {
            cancel: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(DiscoveryState::default())),
        };
        let thread_shared = shared.clone();
        let join = thread::Builder::new()
            .name("discover".to_owned())
            .spawn(move || run_pipeline(&thread_shared, config))
            .expect("failed to spawn discover thread");
        Self {
            cancel: shared.cancel,
            state: shared.state,
            join: Some(join),
        }
    }

    /// Signals cancellation between work units; results accumulated so far
    /// are preserved into the terminal snapshot.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> DiscoverySnapshot {
        let state = lock(&self.state);
        DiscoverySnapshot::from(&*state)
    }
}

impl Drop for DiscoveryHandle {
    fn drop(&mut self) {
        self.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Clone)]
struct HandleShared {
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<DiscoveryState>>,
}

const POLL_INTERVAL: Duration = Duration::from_millis(50);

fn run_pipeline(shared: &HandleShared, config: DiscoveryConfig) {
    // 1. Secondary source first: strictly bounded, degrades to empty on any
    // socket-level failure (REQ-11).
    let ws = wsdiscovery::discover(wsdiscovery::PROBE_WAIT, &shared.cancel);

    // 2. Target list: /24 hosts plus discovered addresses, deduped (REQ-5).
    let mut hosts = net::host_addresses(config.subnet);
    for ip in &ws.addresses {
        if !hosts.contains(ip) {
            hosts.push(*ip);
        }
    }

    {
        let mut state = lock(&shared.state);
        state.phase = Phase::Scanning;
        state.hosts_total = hosts.len() as u32;
        state.ws_found = ws.addresses.len() as u32;
        state.ws_degraded = ws.degraded;
    }

    // 3. TCP scan on its own worker thread while this thread polls progress
    // into the shared state so the UI sees live N/M counters (REQ-4).
    let total_hosts = hosts.len() as u32;
    let ports_count = config.ports.len().max(1) as u32;
    let units = Arc::new(AtomicU32::new(0));
    let scan_hosts = Arc::new(hosts);
    let scan_ports = Arc::new(config.ports.clone());
    let worker = {
        let cancel = Arc::clone(&shared.cancel);
        let units = Arc::clone(&units);
        let hosts = Arc::clone(&scan_hosts);
        let ports = Arc::clone(&scan_ports);
        thread::Builder::new()
            .name("discover-scan".to_owned())
            .spawn(move || scan::scan(&hosts, &ports, &cancel, &units))
            .expect("failed to spawn scan worker")
    };
    while !worker.is_finished() {
        update_scan_progress(shared, &units, ports_count, total_hosts);
        thread::sleep(POLL_INTERVAL);
    }
    let responders_raw = worker.join().unwrap_or_default();
    {
        let mut state = lock(&shared.state);
        state.hosts_scanned = total_hosts;
        state.responders_found = responders_raw.len() as u32;
    }

    // 4. At most one entry per IP before probing (REQ-5).
    let responders = scan::consolidate(responders_raw);

    // 5. Probe pool. Estimated total assumes up to the hard cap per host.
    let probes_total: u32 = responders.len() as u32
        * probe::MAX_ATTEMPTS_PER_HOST.min(probe::VENDOR_PATHS.len()) as u32;
    lock(&shared.state).probes_total = probes_total;

    let probe_units = Arc::new(AtomicU32::new(0));
    let ffprobe_ok = run_probe_phase(shared, responders, &config, &probe_units);

    // Terminal state; partial results survive cancellation (REQ-4).
    let mut state = lock(&shared.state);
    state.probes_done = probe_units.load(Ordering::Relaxed).min(state.probes_total);
    state.phase = if shared.cancel.load(Ordering::Relaxed) {
        Phase::Cancelled
    } else if !ffprobe_ok {
        state.error = Some("ffprobe not found".to_owned());
        Phase::Failed("ffprobe not found".to_owned())
    } else {
        Phase::Complete
    };
}

fn update_scan_progress(
    shared: &HandleShared,
    units: &AtomicU32,
    ports_count: u32,
    total_hosts: u32,
) {
    let scanned = (units.load(Ordering::Relaxed) / ports_count).min(total_hosts);
    lock(&shared.state).hosts_scanned = scanned;
}

fn run_probe_phase(
    shared: &HandleShared,
    responders: Vec<Responder>,
    config: &DiscoveryConfig,
    units: &Arc<AtomicU32>,
) -> bool {
    if responders.is_empty() {
        return true;
    }
    let sink: Arc<Mutex<Vec<DiscoveryResult>>> = Arc::new(Mutex::new(Vec::new()));
    let worker = {
        let cancel = Arc::clone(&shared.cancel);
        let units = Arc::clone(units);
        let sink = Arc::clone(&sink);
        let creds = config.creds.clone();
        let timeout = config.probe_timeout;
        thread::Builder::new()
            .name("discover-probe".to_owned())
            .spawn(move || probe::probe_pool(responders, creds, &cancel, &units, &sink, timeout))
            .expect("failed to spawn probe worker")
    };
    while !worker.is_finished() {
        lock(&shared.state).probes_done = units.load(Ordering::Relaxed);
        thread::sleep(POLL_INTERVAL);
    }
    let ffprobe_ok = worker.join().unwrap_or(false);
    let mut results = lock(&sink);
    results.sort_by_key(|r| r.ip);
    lock(&shared.state).results = std::mem::take(&mut *results);
    ffprobe_ok
}
