use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const PAUSED_PROBE_INTERVAL: Duration = Duration::from_secs(8);
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const STREAM_PROBE_TIMEOUT: Duration = Duration::from_secs(6);
const PAUSED_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Online,
    Offline,
    Paused,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Connecting => "connecting",
            Status::Online => "online",
            Status::Offline => "offline",
            Status::Paused => "paused",
        }
    }
}

#[derive(Debug)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    pub generation: u64,
}

#[derive(Clone)]
struct Shared {
    frame: Arc<Mutex<Option<Arc<Frame>>>>,
    status: Arc<Mutex<Status>>,
    child: Arc<Mutex<Option<Child>>>,
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

pub struct StreamHandle {
    shared: Shared,
}

impl StreamHandle {
    pub fn spawn(url: impl Into<String>) -> Self {
        let shared = Shared {
            frame: Arc::new(Mutex::new(None)),
            status: Arc::new(Mutex::new(Status::Connecting)),
            child: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
        };
        let thread_shared = shared.clone();
        let url = url.into();
        thread::Builder::new()
            .name("cam-stream".to_owned())
            .spawn(move || run_loop(thread_shared, &url))
            .expect("failed to spawn stream thread");
        Self { shared }
    }

    pub fn latest_frame(&self) -> Option<Arc<Frame>> {
        lock(&self.shared.frame).clone()
    }

    pub fn status(&self) -> Status {
        *lock(&self.shared.status)
    }

    pub fn stop(&self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        stop_child(&self.shared.child);
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn stop_child(child_slot: &Mutex<Option<Child>>) {
    if let Some(mut child) = lock(child_slot).take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn sleep_interruptible(shutdown: &AtomicBool, paused: &AtomicBool, total: Duration) {
    let step = Duration::from_millis(250);
    let deadline = Instant::now() + total;
    while !shutdown.load(Ordering::Relaxed)
        && !paused.load(Ordering::Relaxed)
        && Instant::now() < deadline
    {
        thread::sleep(step.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    Success { width: usize, height: usize },
    BadCredentials,
    PathNotFound,
    PortClosed,
    Unreachable,
    NotRtsp,
}

pub fn probe_rtsp(url: &str, timeout: Duration) -> ProbeOutcome {
    let mut command = Command::new("ffprobe");
    command.args([
        "-v",
        "error",
        "-rtsp_transport",
        "tcp",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height",
        "-of",
        "csv=s=x:p=0",
    ]);
    let micros = timeout.as_micros().to_string();
    command.args(["-rw_timeout", &micros, "-timeout", &micros]);
    command.arg(url);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return ProbeOutcome::Unreachable,
    };
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout_bytes = Vec::new();
                let mut stderr_bytes = Vec::new();
                if let Some(pipe) = stdout_pipe.as_mut() {
                    let _ = pipe.read_to_end(&mut stdout_bytes);
                }
                if let Some(pipe) = stderr_pipe.as_mut() {
                    let _ = pipe.read_to_end(&mut stderr_bytes);
                }
                let stdout_text = String::from_utf8_lossy(&stdout_bytes);
                let stderr_text = String::from_utf8_lossy(&stderr_bytes);
                return classify_ffprobe(status.success(), &stdout_text, &stderr_text);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return ProbeOutcome::Unreachable;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return ProbeOutcome::Unreachable;
            }
        }
    }
}

const AUTH_MARKERS: [&str; 3] = ["401", "unauthorized", "authenticat"];
const REFUSED_MARKERS: [&str; 1] = ["connection refused"];
const UNREACHABLE_MARKERS: [&str; 7] = [
    "timed out",
    "timeout",
    "no route",
    "unreachable",
    "network is down",
    "connection reset",
    "connection aborted",
];
const NOT_FOUND_MARKERS: [&str; 2] = ["404", "not found"];
const NOT_RTSP_MARKERS: [&str; 4] = ["invalid data", "unsupported", "could not find codec", "sdp"];

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn parse_dimensions(stdout: &str) -> Option<(usize, usize)> {
    let line = stdout.lines().next()?.trim();
    let (w, h) = line.split_once('x')?;
    let width: usize = w.parse().ok()?;
    let height: usize = h.parse().ok()?;
    if width > 0 && height > 0 {
        Some((width, height))
    } else {
        None
    }
}

pub(crate) fn classify_ffprobe(exit_success: bool, stdout: &str, stderr: &str) -> ProbeOutcome {
    if exit_success && let Some((width, height)) = parse_dimensions(stdout) {
        return ProbeOutcome::Success { width, height };
    }
    let text = format!("{stderr}\n{stdout}").to_lowercase();
    if contains_any(&text, &AUTH_MARKERS) {
        return ProbeOutcome::BadCredentials;
    }
    if contains_any(&text, &REFUSED_MARKERS) {
        return ProbeOutcome::PortClosed;
    }
    if contains_any(&text, &UNREACHABLE_MARKERS) {
        return ProbeOutcome::Unreachable;
    }
    if contains_any(&text, &NOT_FOUND_MARKERS) {
        return ProbeOutcome::PathNotFound;
    }
    if contains_any(&text, &NOT_RTSP_MARKERS) || exit_success {
        return ProbeOutcome::NotRtsp;
    }
    ProbeOutcome::PathNotFound
}

fn run_loop(shared: Shared, url: &str) {
    let mut paused_seen = false;
    while !shared.shutdown.load(Ordering::Relaxed) {
        if shared.paused.load(Ordering::Relaxed) {
            if !paused_seen {
                paused_seen = true;
                stop_child(&shared.child);
                *lock(&shared.status) = Status::Paused;
            }
            let _ = probe_rtsp(url, PAUSED_PROBE_TIMEOUT);
            sleep_interruptible(&shared.shutdown, &shared.paused, PAUSED_PROBE_INTERVAL);
            continue;
        }
        paused_seen = false;
        *lock(&shared.status) = Status::Connecting;

        let dims = match probe_rtsp(url, STREAM_PROBE_TIMEOUT) {
            ProbeOutcome::Success { width, height } => (width, height),
            ProbeOutcome::BadCredentials
            | ProbeOutcome::PathNotFound
            | ProbeOutcome::PortClosed
            | ProbeOutcome::Unreachable
            | ProbeOutcome::NotRtsp => {
                *lock(&shared.status) = Status::Offline;
                sleep_interruptible(&shared.shutdown, &shared.paused, RECONNECT_DELAY);
                continue;
            }
        };
        let frame_len = dims.0 * dims.1 * 3;
        if frame_len > MAX_FRAME_BYTES {
            *lock(&shared.status) = Status::Offline;
            sleep_interruptible(&shared.shutdown, &shared.paused, RECONNECT_DELAY);
            continue;
        }

        let child = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-rtsp_transport", "tcp", "-i", url])
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        let mut child = match child {
            Ok(child) => child,
            Err(_) => {
                *lock(&shared.status) = Status::Offline;
                sleep_interruptible(&shared.shutdown, &shared.paused, RECONNECT_DELAY);
                continue;
            }
        };

        let mut stdout = child.stdout.take();
        *lock(&shared.child) = Some(child);

        if let Some(stdout) = stdout.as_mut() {
            read_frames(&shared, stdout, dims.0, dims.1);
        }

        stop_child(&shared.child);

        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        if shared.paused.load(Ordering::Relaxed) {
            continue;
        }
        *lock(&shared.status) = Status::Offline;
        sleep_interruptible(&shared.shutdown, &shared.paused, RECONNECT_DELAY);
    }
    stop_child(&shared.child);
}

fn read_frames<R: Read>(shared: &Shared, reader: &mut R, width: usize, height: usize) {
    let frame_len = width * height * 3;
    let mut buf = vec![0u8; frame_len];
    let mut generation: u64 = 0;
    loop {
        if shared.shutdown.load(Ordering::Relaxed) || shared.paused.load(Ordering::Relaxed) {
            return;
        }
        match reader.read_exact(&mut buf) {
            Ok(()) => {
                generation = generation.wrapping_add(1);
                let frame = Frame {
                    width,
                    height,
                    rgb: buf.clone(),
                    generation,
                };
                *lock(&shared.frame) = Some(Arc::new(frame));
                *lock(&shared.status) = Status::Online;
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ProbeOutcome, classify_ffprobe, probe_rtsp};
    use std::net::TcpListener;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    fn classify_fail(stderr: &str) -> ProbeOutcome {
        classify_ffprobe(false, "", stderr)
    }

    #[test]
    fn live555_401_unauthorized_is_bad_credentials() {
        let stderr =
            "[tcp @ 0x55d4] Connection to rtsp://cam:554/live failed\nRTSP/1.0 401 Unauthorized";
        assert_eq!(classify_fail(stderr), ProbeOutcome::BadCredentials);
    }

    #[test]
    fn live555_failed_401_beats_sdp_bucket() {
        let stderr =
            "Failed to get a SDP description from URL rtsp://cam:554/: failed: 401 Unauthorized";
        assert_eq!(classify_fail(stderr), ProbeOutcome::BadCredentials);
    }

    #[test]
    fn rtsp_404_describe_rejection_is_path_not_found() {
        let stderr = "server returned 404 Not Found while handling DESCRIBE";
        assert_eq!(classify_fail(stderr), ProbeOutcome::PathNotFound);
    }

    #[test]
    fn connection_refused_is_port_closed() {
        assert_eq!(
            classify_fail("Connection refused"),
            ProbeOutcome::PortClosed
        );
    }

    #[test]
    fn connect_timeout_is_unreachable() {
        assert_eq!(
            classify_fail("Connection timed out"),
            ProbeOutcome::Unreachable
        );
    }

    #[test]
    fn no_route_is_unreachable() {
        assert_eq!(classify_fail("No route to host"), ProbeOutcome::Unreachable);
    }

    #[test]
    fn network_unreachable_is_unreachable() {
        assert_eq!(
            classify_fail("Network is unreachable"),
            ProbeOutcome::Unreachable
        );
    }

    #[test]
    fn connection_reset_is_unreachable() {
        assert_eq!(
            classify_fail("Connection reset by peer"),
            ProbeOutcome::Unreachable
        );
    }

    #[test]
    fn http_endpoint_invalid_data_is_not_rtsp() {
        let stderr =
            "<html><body>400 Bad Request</body></html>\nInvalid data found when processing input";
        assert_eq!(classify_fail(stderr), ProbeOutcome::NotRtsp);
    }

    #[test]
    fn unsupported_marker_is_not_rtsp() {
        assert_eq!(
            classify_fail("Unsupported media type offered"),
            ProbeOutcome::NotRtsp
        );
    }

    #[test]
    fn sdp_failure_without_auth_markers_is_not_rtsp() {
        assert_eq!(
            classify_fail("Failed to get a SDP description from URL rtsp://cam:80/"),
            ProbeOutcome::NotRtsp
        );
    }

    #[test]
    fn could_not_find_codec_is_not_rtsp() {
        assert_eq!(
            classify_fail("Could not find codec parameters for the stream"),
            ProbeOutcome::NotRtsp
        );
    }

    #[test]
    fn exit_ok_but_unparsable_stdout_is_not_rtsp() {
        assert_eq!(
            classify_ffprobe(true, "not-a-dimension", ""),
            ProbeOutcome::NotRtsp
        );
    }

    #[test]
    fn success_csv_parses_dimensions() {
        assert_eq!(
            classify_ffprobe(true, "1920x1080\n", ""),
            ProbeOutcome::Success {
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn residual_failure_text_is_path_not_found() {
        assert_eq!(
            classify_fail("some totally unknown diagnostic xyz"),
            ProbeOutcome::PathNotFound
        );
    }

    #[test]
    fn probe_rtsp_blackhole_listener_fails_within_deadline() {
        let ok_ffprobe = Command::new("ffprobe")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if !ok_ffprobe {
            eprintln!("ffprobe not available, skipping");
            return;
        }

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let holder = thread::spawn(move || {
            // accept then send nothing: black-hole stand-in
            if let Ok((_stream, _)) = listener.accept() {
                thread::sleep(Duration::from_millis(2000));
            }
        });

        let url = format!("rtsp://{addr}/live");
        let timeout = Duration::from_millis(700);
        let start = Instant::now();
        let outcome = probe_rtsp(&url, timeout);
        let elapsed = start.elapsed();

        assert!(
            !matches!(outcome, ProbeOutcome::Success { .. }),
            "black-hole probe must not report success"
        );
        assert!(
            elapsed < timeout + Duration::from_secs(1),
            "probe took {elapsed:?}, expected under {:?}",
            timeout + Duration::from_secs(1)
        );
        holder.join().expect("holder thread finishes");
    }
}
