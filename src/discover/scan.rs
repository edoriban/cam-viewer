//! Bounded TCP-connect port scan over /24 host lists.
//!
//! Worst-case budget (documented, Reconciliation 4): 254 hosts x 5 ports =
//! 1270 tasks through [`SCAN_WORKERS`] workers at [`CONNECT_TIMEOUT`] each
//! is roughly 20 waves x 300 ms = 6 s, under the spec REQ-3 10 s ceiling.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub const DEFAULT_PORTS: &[u16] = &[554, 80, 8554, 8000, 8899];
pub const SCAN_WORKERS: usize = 64;
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

/// A host that accepted a TCP connection on one scanned port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Responder {
    pub ip: Ipv4Addr,
    pub port: u16,
}

/// Scans every `(host, port)` pair with up to [`SCAN_WORKERS`] concurrent
/// connects. Tasks are generated port-major (whole first-port column first)
/// so the highest-yield RTSP port completes early. `cancel` is checked before
/// every connect; silent non-responders (timeout, refusal) are dropped
/// without surfacing errors (spec REQ-3). `progress` counts completed work
/// units monotonically.
pub fn scan(
    hosts: &[Ipv4Addr],
    ports: &[u16],
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Vec<Responder> {
    let (sender, receiver) = mpsc::channel::<(Ipv4Addr, u16)>();
    for port in ports {
        for host in hosts {
            if sender.send((*host, *port)).is_err() {
                break;
            }
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
                    let addr = (ip, port).into();
                    if TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).is_ok() {
                        lock(responders).push(Responder { ip, port });
                    }
                    progress.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    let mut found = lock(&responders);
    found.sort_by_key(|r| (r.ip, r.port));
    std::mem::take(&mut *found)
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
    use super::{Responder, consolidate};
    use std::net::Ipv4Addr;

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
}
