//! Bounded TCP-connect port scan over /24 host lists.
//!
//! Worst-case budget (documented, Reconciliation 4): 254 hosts x 5 ports =
//! 1270 tasks through [`SCAN_WORKERS`] workers at [`CONNECT_TIMEOUT`] each
//! is roughly 20 waves x 300 ms = 6 s. The retry pass adds at most 254 tasks
//! at [`RETRY_CONNECT_TIMEOUT`], roughly 4 waves x 800 ms = 3.2 s, so the
//! whole scan stays under the spec REQ-3 10 s ceiling.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub const DEFAULT_PORTS: &[u16] = &[554, 80, 8554, 8000, 8899];
pub const SCAN_WORKERS: usize = 64;
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

/// Deadline for the second chance given to hosts that answered nothing in
/// the first pass. Wi-Fi cameras routinely take 150-300 ms to accept a
/// connection, and the very first SYN to an idle host also waits for ARP, so
/// a single 300 ms window silently dropped real cameras from the results.
pub const RETRY_CONNECT_TIMEOUT: Duration = Duration::from_millis(800);

/// A host that accepted a TCP connection on one scanned port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Responder {
    pub ip: Ipv4Addr,
    pub port: u16,
}

/// Scans every `(host, port)` pair with up to [`SCAN_WORKERS`] concurrent
/// connects. Tasks are generated port-major (whole first-port column first)
/// so the highest-yield RTSP port completes early. Hosts that answered on no
/// port are then retried once on the first port with the longer
/// [`RETRY_CONNECT_TIMEOUT`]. `cancel` is checked before every connect;
/// silent non-responders (timeout, refusal) are dropped without surfacing
/// errors (spec REQ-3). `progress` counts completed work units monotonically,
/// retries included.
pub fn scan(
    hosts: &[Ipv4Addr],
    ports: &[u16],
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Vec<Responder> {
    let connect =
        |addr: SocketAddr, timeout: Duration| TcpStream::connect_timeout(&addr, timeout).is_ok();
    scan_with(hosts, ports, cancel, progress, &connect)
}

/// [`scan`] over an injected connector, so the two-pass policy is testable
/// without depending on real network timing.
fn scan_with(
    hosts: &[Ipv4Addr],
    ports: &[u16],
    cancel: &AtomicBool,
    progress: &AtomicU32,
    connect: &(dyn Fn(SocketAddr, Duration) -> bool + Sync),
) -> Vec<Responder> {
    let mut tasks = Vec::with_capacity(hosts.len() * ports.len());
    for port in ports {
        for host in hosts {
            tasks.push((*host, *port));
        }
    }
    let mut found = connect_all(tasks, CONNECT_TIMEOUT, cancel, progress, connect);

    if let Some(first_port) = ports.first()
        && !cancel.load(Ordering::Relaxed)
    {
        let answered: BTreeSet<Ipv4Addr> = found.iter().map(|r| r.ip).collect();
        let retries = hosts
            .iter()
            .filter(|host| !answered.contains(host))
            .map(|host| (*host, *first_port))
            .collect();
        found.extend(connect_all(
            retries,
            RETRY_CONNECT_TIMEOUT,
            cancel,
            progress,
            connect,
        ));
    }

    found.sort_by_key(|r| (r.ip, r.port));
    found
}

/// Runs `tasks` through [`SCAN_WORKERS`] workers, each connect bounded by
/// `timeout`, and returns the pairs that accepted.
fn connect_all(
    tasks: Vec<(Ipv4Addr, u16)>,
    timeout: Duration,
    cancel: &AtomicBool,
    progress: &AtomicU32,
    connect: &(dyn Fn(SocketAddr, Duration) -> bool + Sync),
) -> Vec<Responder> {
    let (sender, receiver) = mpsc::channel::<(Ipv4Addr, u16)>();
    for task in tasks {
        if sender.send(task).is_err() {
            break;
        }
    }
    drop(sender);

    let responders = Mutex::new(Vec::<Responder>::new());
    let receiver = Mutex::new(receiver);
    thread::scope(|scope| {
        let receiver = &receiver;
        let responders = &responders;
        for _ in 0..SCAN_WORKERS {
            scope.spawn(move || {
                loop {
                    let task = {
                        let queue = lock(receiver);
                        queue.recv()
                    };
                    let Ok((ip, port)) = task else {
                        break;
                    };
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    if connect((ip, port).into(), timeout) {
                        lock(responders).push(Responder { ip, port });
                    }
                    progress.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    std::mem::take(&mut *lock(&responders))
}

/// Consolidates responders to at most one entry per IP: port 554 when it
/// responded, otherwise the lowest responding port (spec REQ-5).
pub fn consolidate(responders: Vec<Responder>) -> Vec<Responder> {
    let mut by_ip: BTreeMap<Ipv4Addr, Vec<u16>> = BTreeMap::new();
    for responder in responders {
        by_ip.entry(responder.ip).or_default().push(responder.port);
    }
    by_ip
        .into_iter()
        .map(|(ip, mut ports)| {
            ports.sort_unstable();
            let port = if ports.contains(&554) { 554 } else { ports[0] };
            Responder { ip, port }
        })
        .collect()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{RETRY_CONNECT_TIMEOUT, Responder, consolidate, lock, scan_with};
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Duration;

    fn responder(octets: [u8; 4], port: u16) -> Responder {
        Responder {
            ip: Ipv4Addr::from(octets),
            port,
        }
    }

    #[test]
    fn multi_port_host_consolidates_to_single_entry_preferring_554() {
        let consolidated = consolidate(vec![
            responder([192, 168, 1, 64], 8554),
            responder([192, 168, 1, 64], 554),
            responder([192, 168, 1, 64], 80),
        ]);
        assert_eq!(consolidated, vec![responder([192, 168, 1, 64], 554)]);
    }

    #[test]
    fn host_without_554_takes_lowest_responding_port() {
        let consolidated = consolidate(vec![
            responder([192, 168, 1, 77], 8554),
            responder([192, 168, 1, 77], 8000),
        ]);
        assert_eq!(consolidated, vec![responder([192, 168, 1, 77], 8000)]);
    }

    #[test]
    fn single_port_host_is_identity() {
        let consolidated = consolidate(vec![responder([10, 0, 0, 5], 8899)]);
        assert_eq!(consolidated, vec![responder([10, 0, 0, 5], 8899)]);
    }

    #[test]
    fn distinct_hosts_each_keep_one_entry() {
        let consolidated = consolidate(vec![
            responder([10, 0, 0, 5], 554),
            responder([10, 0, 0, 6], 80),
            responder([10, 0, 0, 6], 554),
        ]);
        assert_eq!(
            consolidated,
            vec![responder([10, 0, 0, 5], 554), responder([10, 0, 0, 6], 554),]
        );
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert!(consolidate(Vec::new()).is_empty());
    }

    /// Connector that only "answers" when given at least `needs`, standing in
    /// for a Wi-Fi camera whose first SYN loses the race to ARP or power save.
    fn slow_host(slow: Ipv4Addr, needs: Duration) -> impl Fn(SocketAddr, Duration) -> bool + Sync {
        move |addr, timeout| match addr {
            SocketAddr::V4(v4) if *v4.ip() == slow => timeout >= needs,
            _ => false,
        }
    }

    #[test]
    fn host_missing_the_first_connect_window_is_found_by_the_retry_pass() {
        let slow = Ipv4Addr::new(192, 168, 100, 5);
        let hosts = [Ipv4Addr::new(192, 168, 100, 4), slow];
        let connect = slow_host(slow, RETRY_CONNECT_TIMEOUT);

        let found = scan_with(
            &hosts,
            &[554, 80],
            &AtomicBool::new(false),
            &AtomicU32::new(0),
            &connect,
        );

        assert_eq!(found, vec![responder([192, 168, 100, 5], 554)]);
    }

    #[test]
    fn retry_pass_skips_hosts_that_already_answered_and_only_retries_the_first_port() {
        let answered = Ipv4Addr::new(10, 0, 0, 5);
        let silent = Ipv4Addr::new(10, 0, 0, 6);
        let calls = Mutex::new(Vec::new());
        let connect = |addr: SocketAddr, timeout: Duration| {
            lock(&calls).push((addr, timeout));
            matches!(addr, SocketAddr::V4(v4) if *v4.ip() == answered && v4.port() == 80)
        };
        let progress = AtomicU32::new(0);

        let found = scan_with(
            &[answered, silent],
            &[554, 80],
            &AtomicBool::new(false),
            &progress,
            &connect,
        );

        assert_eq!(found, vec![responder([10, 0, 0, 5], 80)]);
        let retries: Vec<SocketAddr> = lock(&calls)
            .iter()
            .filter(|(_, timeout)| *timeout == RETRY_CONNECT_TIMEOUT)
            .map(|(addr, _)| *addr)
            .collect();
        assert_eq!(retries, vec![SocketAddr::from((silent, 554))]);
        // Four first-pass units plus the single retry.
        assert_eq!(progress.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn retry_pass_is_skipped_once_cancelled() {
        let calls = AtomicU32::new(0);
        let connect = |_: SocketAddr, _: Duration| {
            calls.fetch_add(1, Ordering::Relaxed);
            false
        };
        let cancel = AtomicBool::new(true);

        let found = scan_with(
            &[Ipv4Addr::new(10, 0, 0, 7)],
            &[554],
            &cancel,
            &AtomicU32::new(0),
            &connect,
        );

        assert!(found.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
}
