use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const PAUSED_PROBE_INTERVAL: Duration = Duration::from_secs(8);
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Socket deadline handed to the streaming ffmpeg. Without a pre-flight probe
/// this is the only thing that unsticks a camera that accepts the connection
/// and then goes silent mid-stream.
const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(10);
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingState {
    Idle,
    Recording { path: PathBuf, started: Instant },
    Saving { path: PathBuf },
    Saved { path: PathBuf, at: Instant },
    Failed(String),
}

/// Bound on how long the background finish thread `stop()` (app shutdown /
/// `Drop`) starts waits for ffmpeg to finish writing the file after asking
/// it to quit, before giving up and killing it. Kept short relative to
/// `RECORDING_FINISH_TIMEOUT` because `CamViewerApp::on_exit` waits
/// (bounded, via `join_finish`) for every camera's thread together before
/// the app actually quits.
const RECORDING_STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// Same bound for an interactive STOP click's background finish thread,
/// longer since nothing is waiting on it synchronously.
const RECORDING_FINISH_TIMEOUT: Duration = Duration::from_secs(8);

/// A running recorder: the ffmpeg child plus the plumbing `finish_recording`
/// needs to stop it cleanly and judge the outcome.
struct Recorder {
    child: Child,
    path: PathBuf,
    started: Instant,
    stderr_tail: Arc<Mutex<String>>,
    stderr_thread: thread::JoinHandle<()>,
}

#[derive(Clone)]
struct Shared {
    frame: Arc<Mutex<Option<Arc<Frame>>>>,
    status: Arc<Mutex<Status>>,
    child: Arc<Mutex<Option<Child>>>,
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    url: String,
    recorder: Arc<Mutex<Option<Recorder>>>,
    recording_state: Arc<Mutex<RecordingState>>,
    audio_child: Arc<Mutex<Option<Child>>>,
    /// Reconnect attempts since the last successfully published frame; reset
    /// to 0 by every frame `read_frames` publishes, not just the first ever.
    attempts: Arc<AtomicU32>,
    /// Deadline of the current reconnect backoff sleep, `Some` only while
    /// actually waiting in it.
    next_retry: Arc<Mutex<Option<Instant>>>,
    /// Start time of the current phase (a reconnect attempt, or the stream
    /// being open with no frame yet), used to render elapsed time in tiles.
    phase_started: Arc<Mutex<Instant>>,
    /// Set once `read_frames` has published a frame during the current
    /// attempt; cleared by `begin_attempt` so a stale frame from a previous
    /// attempt never makes a fresh reconnect look reachable (the texture
    /// itself is still kept, see `read_frames`).
    has_frame: Arc<AtomicBool>,
    /// Set by `StreamHandle::retry_now` to wake an in-progress backoff sleep
    /// immediately; consumed (cleared) by `sleep_interruptible` itself.
    retry_wake: Arc<AtomicBool>,
    /// Handle for an in-flight background recorder-finish thread, started by
    /// `stop_recording` or by `StreamHandle::stop` signalling shutdown while
    /// a recording was active. `StreamHandle::join_finish` takes and joins
    /// it, bounded by the thread's own `finish_recording` timeout, so app
    /// exit never orphans an ffmpeg process still finalizing a file.
    finish_thread: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

pub struct StreamHandle {
    shared: Shared,
}

impl StreamHandle {
    pub fn spawn(url: impl Into<String>, muted: bool) -> Self {
        let url = url.into();
        let shared = Shared {
            frame: Arc::new(Mutex::new(None)),
            status: Arc::new(Mutex::new(Status::Connecting)),
            child: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            muted: Arc::new(AtomicBool::new(muted)),
            url: url.clone(),
            recorder: Arc::new(Mutex::new(None)),
            recording_state: Arc::new(Mutex::new(RecordingState::Idle)),
            audio_child: Arc::new(Mutex::new(None)),
            attempts: Arc::new(AtomicU32::new(0)),
            next_retry: Arc::new(Mutex::new(None)),
            phase_started: Arc::new(Mutex::new(Instant::now())),
            has_frame: Arc::new(AtomicBool::new(false)),
            retry_wake: Arc::new(AtomicBool::new(false)),
            finish_thread: Arc::new(Mutex::new(None)),
        };
        let thread_shared = shared.clone();
        thread::Builder::new()
            .name("cam-stream".to_owned())
            .spawn(move || run_loop(thread_shared, &url))
            .expect("failed to spawn stream thread");
        let audio_shared = shared.clone();
        let audio_url = shared.url.clone();
        thread::Builder::new()
            .name("cam-audio".to_owned())
            .spawn(move || audio_loop(audio_shared, &audio_url))
            .expect("failed to spawn audio thread");
        Self { shared }
    }

    pub fn latest_frame(&self) -> Option<Arc<Frame>> {
        lock(&self.shared.frame).clone()
    }

    pub fn status(&self) -> Status {
        *lock(&self.shared.status)
    }

    /// Reconnect attempts since the last frame this stream published.
    pub fn attempts(&self) -> u32 {
        self.shared.attempts.load(Ordering::Relaxed)
    }

    /// Deadline of the current reconnect backoff, if one is in progress.
    pub fn next_retry_at(&self) -> Option<Instant> {
        *lock(&self.shared.next_retry)
    }

    /// Start time of the current phase (see [`Shared::phase_started`]).
    pub fn phase_started_at(&self) -> Instant {
        *lock(&self.shared.phase_started)
    }

    /// Whether this stream has ever published a frame.
    pub fn has_frame(&self) -> bool {
        self.shared.has_frame.load(Ordering::Relaxed)
    }

    /// Wakes an in-progress reconnect backoff immediately instead of waiting
    /// out the rest of `RECONNECT_DELAY`.
    pub fn retry_now(&self) {
        self.shared.retry_wake.store(true, Ordering::Relaxed);
    }

    /// Signals this stream (and any active recorder) to stop, without
    /// blocking the caller: used by `Drop` (removing or editing a camera via
    /// `apply_cameras` runs this on the UI thread) and by app exit's first
    /// pass over every camera (see `CamViewerApp::on_exit`), which then
    /// calls `join_finish` on all of them together instead of waiting here
    /// one camera at a time.
    pub fn stop(&self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        if let Some(recorder) = lock(&self.shared.recorder).take() {
            self.spawn_finish(recorder, RECORDING_STOP_TIMEOUT);
        }
        stop_child(&self.shared.child);
        stop_child(&self.shared.audio_child);
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.shared.muted.load(Ordering::Relaxed)
    }

    /// Polls the recording child, if any, and settles an unrequested exit
    /// (ffmpeg dying or refusing to start) into `Failed` before returning the
    /// current state. Called every frame by the UI.
    pub fn recording_state(&self) -> RecordingState {
        let mut slot = lock(&self.shared.recorder);
        if let Some(recorder) = slot.as_mut()
            && let Ok(Some(status)) = recorder.child.try_wait()
        {
            let tail = lock(&recorder.stderr_tail).clone();
            let settled = settle_exit(&recorder.path, status.success(), false, &tail);
            slot.take();
            *lock(&self.shared.recording_state) = settled;
        }
        lock(&self.shared.recording_state).clone()
    }

    /// Shows why a recording could not even be started (for example, its
    /// folder could not be created). Ignored while a recording is running.
    pub fn report_recording_error(&self, message: String) {
        if lock(&self.shared.recorder).is_none() {
            *lock(&self.shared.recording_state) = RecordingState::Failed(message);
        }
    }

    pub fn start_recording(&self, path: impl Into<String>) -> Result<(), String> {
        // Keep the duplicate check, process creation, and child publication
        // under one mutex. Otherwise two callers can both observe Idle and
        // spawn separate ffmpeg children before either publishes its child.
        let mut slot = lock(&self.shared.recorder);

        if let Some(recorder) = slot.as_mut()
            && let Ok(Some(status)) = recorder.child.try_wait()
        {
            let tail = lock(&recorder.stderr_tail).clone();
            let settled = settle_exit(&recorder.path, status.success(), false, &tail);
            slot.take();
            *lock(&self.shared.recording_state) = settled;
        }

        if let Some(recorder) = slot.as_ref() {
            // Already recording: idempotent, matching the previous state.
            *lock(&self.shared.recording_state) = RecordingState::Recording {
                path: recorder.path.clone(),
                started: recorder.started,
            };
            return Ok(());
        }

        if matches!(
            *lock(&self.shared.recording_state),
            RecordingState::Saving { .. }
        ) {
            return Err("a recording is still being saved".to_owned());
        }

        let path = PathBuf::from(path.into());
        let args = recording_args(&self.shared.url, &path);
        let recorder = match spawn_recorder(&args, path.clone()) {
            Ok(recorder) => recorder,
            Err(error) => {
                *lock(&self.shared.recording_state) = RecordingState::Failed(error.clone());
                return Err(error);
            }
        };
        let started = recorder.started;
        *slot = Some(recorder);
        *lock(&self.shared.recording_state) = RecordingState::Recording { path, started };
        Ok(())
    }

    /// Takes the recorder off the shared slot, publishes `Saving`
    /// immediately, and finishes ffmpeg on a background thread so the UI
    /// stays responsive while the file is finalized.
    pub fn stop_recording(&self) {
        let mut recorder = match lock(&self.shared.recorder).take() {
            Some(recorder) => recorder,
            None => return,
        };
        // An ffmpeg that already exited on its own (e.g. refused to start,
        // or a bounded clip that finished by itself) is an unrequested exit
        // even though STOP is being pressed now: judging it as a requested
        // stop would read a coincidental non-empty file as Saved.
        if let Ok(Some(status)) = recorder.child.try_wait() {
            let tail = lock(&recorder.stderr_tail).clone();
            let settled = settle_exit(&recorder.path, status.success(), false, &tail);
            *lock(&self.shared.recording_state) = settled;
            return;
        }
        self.spawn_finish(recorder, RECORDING_FINISH_TIMEOUT);
    }

    /// Publishes `Saving` and finishes `recorder` on a background thread
    /// (never the caller's), keeping its `JoinHandle` in `Shared` so
    /// `join_finish` can wait for it later without blocking here.
    fn spawn_finish(&self, recorder: Recorder, timeout: Duration) {
        *lock(&self.shared.recording_state) = RecordingState::Saving {
            path: recorder.path.clone(),
        };
        let state_slot = Arc::clone(&self.shared.recording_state);
        let handle = thread::Builder::new()
            .name("cam-recorder-finish".to_owned())
            .spawn(move || {
                let settled = finish_recording(recorder, timeout);
                *lock(&state_slot) = settled;
            })
            .expect("failed to spawn recorder-finish thread");
        *lock(&self.shared.finish_thread) = Some(handle);
    }

    /// Bounded wait for a background recorder-finish thread started by
    /// `stop` or an earlier `stop_recording`, so app exit never orphans an
    /// ffmpeg process that is still finalizing a file. A no-op if nothing is
    /// in flight; bounded by that thread's own `finish_recording` timeout.
    pub fn join_finish(&self) {
        if let Some(handle) = lock(&self.shared.finish_thread).take() {
            let _ = handle.join();
        }
    }

    pub fn grab_start(&self, path: impl Into<String>) -> Result<(), String> {
        self.start_recording(path)
    }

    pub fn grab_stop(&self) {
        self.stop_recording();
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Builds a `Command` that never opens a console window on Windows.
///
/// Every ffmpeg/ffprobe spawn must go through here: a GUI process spawning a
/// console subsystem child makes Windows allocate a console for it, which
/// flashes a black window on screen (once per reconnect, per camera).
#[cfg(windows)]
pub(crate) fn no_window_command(program: &str) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(windows))]
pub(crate) fn no_window_command(program: &str) -> Command {
    Command::new(program)
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

/// ffmpeg arguments for a recording (S1/S5): `-n` refuses to clobber an
/// existing file (names are unique already, this is a last-resort guard),
/// audio is always mapped regardless of the camera's mute state, and the
/// mixed codecs (copy video, re-encode audio to AAC) are what MP4 can carry
/// from a camera that typically sends G.711. No `-nostdin`: stdin is how
/// `finish_recording` asks ffmpeg to stop cleanly.
fn recording_args(url: &str, path: &Path) -> Vec<String> {
    let timeout = STREAM_IO_TIMEOUT.as_micros().to_string();
    let path_str = path.display().to_string();
    [
        "-v",
        "error",
        "-n",
        "-rtsp_transport",
        "tcp",
        "-timeout",
        timeout.as_str(),
        "-i",
        url,
        "-map",
        "0:v:0",
        "-map",
        "0:a?",
        "-c:v",
        "copy",
        "-c:a",
        "aac",
        "-f",
        "mp4",
        path_str.as_str(),
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Spawns ffmpeg for a recording: stdin piped (so `finish_recording` can send
/// `q`), stdout discarded, and stderr piped to a thread that keeps only the
/// last non-empty line so the pipe never fills and backs ffmpeg up.
fn spawn_recorder(args: &[String], path: PathBuf) -> Result<Recorder, String> {
    let mut command = no_window_command("ffmpeg");
    command.args(args);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("starting recording: {error}"))?;
    let stderr = child.stderr.take().expect("stderr piped above");
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let tail_for_thread = Arc::clone(&stderr_tail);
    let stderr_thread = thread::Builder::new()
        .name("cam-recorder-stderr".to_owned())
        .spawn(move || drain_stderr(stderr, &tail_for_thread))
        .expect("failed to spawn recorder-stderr thread");
    Ok(Recorder {
        child,
        path,
        started: Instant::now(),
        stderr_tail,
        stderr_thread,
    })
}

fn drain_stderr(stderr: impl Read, tail: &Mutex<String>) {
    let reader = BufReader::new(stderr);
    for line in reader.lines().map_while(Result::ok) {
        if !line.trim().is_empty() {
            *lock(tail) = line;
        }
    }
}

/// Judges what a finished ffmpeg recording child amounts to. `success` (the
/// process exit status) is deliberately not used to decide between Saved and
/// Failed: an exit nobody asked for is a failure even at status 0 (e.g.
/// ffmpeg's own `-n` "file already exists" refusal), and a requested stop is
/// judged by whether a non-empty file actually landed on disk, since a
/// `q`-triggered stop's exit code is not a reliable signal either way.
fn settle_exit(path: &Path, _success: bool, requested: bool, stderr_tail: &str) -> RecordingState {
    if !requested {
        return if stderr_tail.is_empty() {
            RecordingState::Failed("recording stopped unexpectedly".to_owned())
        } else {
            RecordingState::Failed(stderr_tail.to_owned())
        };
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > 0 => RecordingState::Saved {
            path: path.to_path_buf(),
            at: Instant::now(),
        },
        _ => RecordingState::Failed("nothing was recorded".to_owned()),
    }
}

/// How long `finish_recording` waits for `q` alone before escalating to
/// SIGINT (unix only). A stalled input (ffmpeg blocked reading a camera
/// that went silent, or a FIFO nobody writes to) never reaches the point of
/// reading stdin, so `q` alone can leave it running for the full `timeout`
/// (measured at 27s against a silent FIFO input before this).
#[cfg(unix)]
const SIGINT_ESCALATION_DELAY: Duration = Duration::from_secs(2);

/// Stops a recorder cleanly (S1): sends `q` on stdin, drops it, and polls
/// `try_wait` until `timeout`. On unix, a child still running after
/// `SIGINT_ESCALATION_DELAY` gets `SIGINT` next: ffmpeg's interrupt callback
/// aborts a blocking read, which a stalled input otherwise leaves deaf to
/// `q` forever. SIGKILL is only the last resort at `timeout`, since a killed
/// ffmpeg may leave the MP4 unfinalized.
fn finish_recording(mut recorder: Recorder, timeout: Duration) -> RecordingState {
    if let Some(mut stdin) = recorder.child.stdin.take() {
        let _ = stdin.write_all(b"q");
    }
    let start = Instant::now();
    let deadline = start + timeout;
    let mut exit_status = None;
    #[cfg(unix)]
    let mut sigint_sent = false;
    loop {
        match recorder.child.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    break;
                }
                #[cfg(unix)]
                if !sigint_sent && start.elapsed() >= SIGINT_ESCALATION_DELAY {
                    sigint_sent = true;
                    send_sigint(recorder.child.id());
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }
    let killed = exit_status.is_none();
    if killed {
        let _ = recorder.child.kill();
        let _ = recorder.child.wait();
    }
    let _ = recorder.stderr_thread.join();
    let tail = lock(&recorder.stderr_tail).clone();
    if killed {
        let suffix = if tail.is_empty() {
            String::new()
        } else {
            format!(": {tail}")
        };
        return RecordingState::Failed(format!(
            "recording did not stop in time and was force-stopped; the file may be incomplete{suffix}"
        ));
    }
    let success = exit_status.expect("checked above").success();
    settle_exit(&recorder.path, success, true, &tail)
}

/// Interrupts a child by pid via the `kill` command rather than an unsafe
/// libc call; ffmpeg treats SIGINT like a clean `q` and aborts any blocking
/// read from its interrupt callback.
#[cfg(unix)]
fn send_sigint(pid: u32) {
    let _ = Command::new("kill")
        .args(["-s", "INT", &pid.to_string()])
        .status();
}

/// Sleeps up to `total`, waking early on shutdown, pause, or `retry_wake`
/// (consumed here so it never cancels a later, unrelated sleep).
fn sleep_interruptible(
    shutdown: &AtomicBool,
    paused: &AtomicBool,
    retry_wake: &AtomicBool,
    total: Duration,
) {
    let step = Duration::from_millis(250);
    let deadline = Instant::now() + total;
    while !shutdown.load(Ordering::Relaxed)
        && !paused.load(Ordering::Relaxed)
        && !retry_wake.load(Ordering::Relaxed)
        && Instant::now() < deadline
    {
        thread::sleep(step.min(deadline.saturating_duration_since(Instant::now())));
    }
    retry_wake.store(false, Ordering::Relaxed);
}

/// Runs immediately before each reconnect attempt (T3): bumps the attempt
/// count, clears any stale retry deadline, resets the phase clock so tiles
/// can render "elapsed since this attempt began", and clears `has_frame` so
/// a frame published by a previous attempt never makes this new attempt
/// look reachable before it proves so. Kept as its own function so the
/// accounting is unit-testable without spawning ffmpeg.
fn begin_attempt(shared: &Shared) {
    shared.attempts.fetch_add(1, Ordering::Relaxed);
    *lock(&shared.next_retry) = None;
    *lock(&shared.phase_started) = Instant::now();
    *lock(&shared.status) = Status::Connecting;
    shared.has_frame.store(false, Ordering::Relaxed);
}

/// Runs after a failed attempt (spawn failure or the stream ending): moves
/// to Offline, schedules `next_retry_at`, and sleeps the backoff —
/// interruptibly by shutdown, pause, or `StreamHandle::retry_now`. `delay`
/// is a parameter (rather than reading `RECONNECT_DELAY` directly) so tests
/// can exercise this with a short, deterministic duration.
/// Runs once ffmpeg's process has spawned (R3 correction): a spawned
/// process only proves the local ffmpeg binary launched, not that the
/// camera itself is reachable, so status is deliberately left as
/// `Connecting` (set by `begin_attempt`) until `read_frames` publishes the
/// first frame of this attempt. `tile_band`'s Online-without-a-frame branch
/// is kept only as a defensive fallback and should no longer be reached in
/// practice. Kept as its own function, even though it currently does
/// nothing, so a regression here (re-promoting status to `Online`) fails a
/// fast unit test instead of only showing up against a real camera.
fn on_process_spawned(shared: &Shared) {
    let _ = shared;
}

fn enter_backoff(shared: &Shared, delay: Duration) {
    *lock(&shared.status) = Status::Offline;
    *lock(&shared.next_retry) = Some(Instant::now() + delay);
    sleep_interruptible(&shared.shutdown, &shared.paused, &shared.retry_wake, delay);
    *lock(&shared.next_retry) = None;
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
    let mut command = no_window_command("ffprobe");
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
            sleep_interruptible(
                &shared.shutdown,
                &shared.paused,
                &shared.retry_wake,
                PAUSED_PROBE_INTERVAL,
            );
            continue;
        }
        paused_seen = false;
        begin_attempt(&shared);

        let io_timeout = STREAM_IO_TIMEOUT.as_micros().to_string();
        let child = no_window_command("ffmpeg")
            .args(["-loglevel", "error", "-rtsp_transport", "tcp"])
            // Deliberately NO -fflags nobuffer / -flags low_delay / -probesize
            // / -analyzeduration here. Measured against real cameras they make
            // first-frame latency worse, not better: starving the demuxer's
            // buffer forces it to wait for the next real keyframe (~2.1s ->
            // ~4.4s), and -probesize 32 fails outright roughly half the time
            // with "Output file does not contain any stream", which costs a
            // full RECONNECT_DELAY before the retry.
            //
            // The rtsp demuxer exposes -timeout (socket I/O, microseconds);
            // -rw_timeout is an ffprobe-only spelling and makes the ffmpeg CLI
            // refuse the input outright with "Option rw_timeout not found".
            .args(["-timeout", &io_timeout])
            .args(["-i", url])
            // PPM states width/height in every frame header, so streaming no
            // longer needs an ffprobe round-trip to learn the geometry and a
            // mid-stream resolution change self-corrects instead of tearing.
            .args(["-f", "image2pipe", "-vcodec", "ppm", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        let mut child = match child {
            Ok(child) => child,
            Err(_) => {
                enter_backoff(&shared, RECONNECT_DELAY);
                continue;
            }
        };

        on_process_spawned(&shared);

        let mut stdout = child.stdout.take();
        *lock(&shared.child) = Some(child);

        if let Some(stdout) = stdout.as_mut() {
            read_frames(&shared, stdout);
        }

        stop_child(&shared.child);

        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        if shared.paused.load(Ordering::Relaxed) {
            continue;
        }
        enter_backoff(&shared, RECONNECT_DELAY);
    }
    stop_child(&shared.child);
}

/// Mirrors `run_loop`'s reconnect lifecycle for a second, independent ffmpeg
/// process that decodes only the audio track and plays it through the
/// default output device. Gated by the `muted` flag.
fn audio_loop(shared: Shared, url: &str) {
    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            return;
        }
        if shared.paused.load(Ordering::Relaxed) || shared.muted.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(250));
            continue;
        }

        let sink = match rodio::DeviceSinkBuilder::open_default_sink() {
            Ok(sink) => sink,
            // No output device is not a transient failure; retrying would
            // just spawn ffmpeg processes nobody can hear.
            Err(_) => return,
        };
        let player = rodio::Player::connect_new(sink.mixer());

        let io_timeout = STREAM_IO_TIMEOUT.as_micros().to_string();
        let child = no_window_command("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-rtsp_transport", "tcp"])
            .args(["-timeout", &io_timeout])
            .args(["-i", url])
            .args([
                "-map", "0:a?", "-vn", "-ac", "1", "-ar", "16000", "-f", "f32le", "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        let mut child = match child {
            Ok(child) => child,
            Err(_) => {
                audio_sleep_interruptible(&shared, RECONNECT_DELAY);
                continue;
            }
        };

        let mut stdout = child.stdout.take();
        *lock(&shared.audio_child) = Some(child);

        if let Some(stdout) = stdout.as_mut() {
            stream_audio_samples(&shared, stdout, &player);
        }

        stop_child(&shared.audio_child);

        if shared.shutdown.load(Ordering::Relaxed) {
            return;
        }
        audio_sleep_interruptible(&shared, RECONNECT_DELAY);
    }
}

/// Like `sleep_interruptible`, but also wakes early when the camera is
/// muted mid-wait instead of finishing out a stale reconnect delay.
fn audio_sleep_interruptible(shared: &Shared, total: Duration) {
    let step = Duration::from_millis(200);
    let deadline = Instant::now() + total;
    while !shared.shutdown.load(Ordering::Relaxed)
        && !shared.paused.load(Ordering::Relaxed)
        && !shared.muted.load(Ordering::Relaxed)
        && Instant::now() < deadline
    {
        thread::sleep(step.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Reads raw interleaved `f32le` PCM from ffmpeg's stdout and appends it to
/// `player` in small chunks as it arrives, so playback starts as soon as the
/// first bytes land instead of waiting to buffer a whole clip. Returns on
/// shutdown, pause, mute, or the ffmpeg process exiting.
fn stream_audio_samples(shared: &Shared, stdout: &mut impl Read, player: &rodio::Player) {
    let mut raw = [0u8; 8192];
    let mut leftover: Vec<u8> = Vec::new();
    loop {
        if shared.shutdown.load(Ordering::Relaxed)
            || shared.paused.load(Ordering::Relaxed)
            || shared.muted.load(Ordering::Relaxed)
        {
            return;
        }
        let read = match stdout.read(&mut raw) {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        leftover.extend_from_slice(&raw[..read]);
        let usable = leftover.len() - (leftover.len() % 4);
        if usable == 0 {
            continue;
        }
        let samples: Vec<f32> = leftover[..usable]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        leftover.drain(..usable);
        player.append(rodio::buffer::SamplesBuffer::new(
            rodio::nz!(1),
            rodio::nz!(16_000),
            samples,
        ));
    }
}

/// Reads one whitespace-delimited PPM header token, skipping leading
/// whitespace and `#` comment lines. Consumes exactly one whitespace byte
/// after the token, which is the single separator PPM mandates before the
/// binary payload. `None` on EOF or on a token long enough to be garbage
/// rather than a header field.
fn next_header_token<R: BufRead>(reader: &mut R) -> Option<String> {
    const MAX_TOKEN_LEN: usize = 20;
    let mut token = String::new();
    let mut byte = [0u8; 1];
    loop {
        reader.read_exact(&mut byte).ok()?;
        if byte[0] == b'#' {
            loop {
                reader.read_exact(&mut byte).ok()?;
                if byte[0] == b'\n' {
                    break;
                }
            }
            continue;
        }
        if byte[0].is_ascii_whitespace() {
            if token.is_empty() {
                continue;
            }
            return Some(token);
        }
        token.push(byte[0] as char);
        if token.len() > MAX_TOKEN_LEN {
            return None;
        }
    }
}

/// Pixel dimensions from one `P6` header, or `None` when the stream is not a
/// PPM frame we can decode.
///
/// Rejects a `maxval` other than 255: larger values mean 16-bit samples, a
/// payload layout twice the size this reader assumes. Also rejects zero and
/// oversized geometry, so a corrupt header can never drive an absurd
/// allocation off a byte stream we do not control.
fn read_ppm_header<R: BufRead>(reader: &mut R) -> Option<(usize, usize)> {
    if next_header_token(reader)? != "P6" {
        return None;
    }
    let width: usize = next_header_token(reader)?.parse().ok()?;
    let height: usize = next_header_token(reader)?.parse().ok()?;
    let maxval: u32 = next_header_token(reader)?.parse().ok()?;
    if maxval != 255 || width == 0 || height == 0 {
        return None;
    }
    let frame_len = width.checked_mul(height)?.checked_mul(3)?;
    if frame_len > MAX_FRAME_BYTES {
        return None;
    }
    Some((width, height))
}

/// Publishes frames until the pipe ends, the stream is stopped, or a header
/// fails to parse. Geometry is read per frame from the PPM header rather than
/// fixed up-front, so a camera that changes resolution mid-stream is followed
/// instead of producing torn frames.
fn read_frames<R: Read>(shared: &Shared, reader: R) {
    let mut reader = BufReader::with_capacity(64 * 1024, reader);
    let mut buf: Vec<u8> = Vec::new();
    let mut generation: u64 = 0;
    loop {
        if shared.shutdown.load(Ordering::Relaxed) || shared.paused.load(Ordering::Relaxed) {
            return;
        }
        let Some((width, height)) = read_ppm_header(&mut reader) else {
            return;
        };
        buf.resize(width * height * 3, 0);
        if reader.read_exact(&mut buf).is_err() {
            return;
        }
        generation = generation.wrapping_add(1);
        let frame = Frame {
            width,
            height,
            rgb: buf.clone(),
            generation,
        };
        *lock(&shared.frame) = Some(Arc::new(frame));
        *lock(&shared.status) = Status::Online;
        shared.has_frame.store(true, Ordering::Relaxed);
        // Every frame resets the failure streak, not just the first one
        // ever: this is "attempts since the last successful frame".
        shared.attempts.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod ppm_tests {
    use super::{
        MAX_FRAME_BYTES, Shared, Status, StreamHandle, begin_attempt, enter_backoff,
        on_process_spawned, read_frames, read_ppm_header, sleep_interruptible,
    };
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn header(bytes: &[u8]) -> Option<(usize, usize)> {
        read_ppm_header(&mut Cursor::new(bytes.to_vec()))
    }

    fn test_shared() -> Shared {
        Shared {
            frame: Arc::new(Mutex::new(None)),
            status: Arc::new(Mutex::new(Status::Connecting)),
            child: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            muted: Arc::new(AtomicBool::new(true)),
            url: "rtsp://127.0.0.1:1/none".to_owned(),
            recorder: Arc::new(Mutex::new(None)),
            recording_state: Arc::new(Mutex::new(super::RecordingState::Idle)),
            audio_child: Arc::new(Mutex::new(None)),
            attempts: Arc::new(AtomicU32::new(0)),
            next_retry: Arc::new(Mutex::new(None)),
            phase_started: Arc::new(Mutex::new(Instant::now())),
            has_frame: Arc::new(AtomicBool::new(false)),
            retry_wake: Arc::new(AtomicBool::new(false)),
            finish_thread: Arc::new(Mutex::new(None)),
        }
    }

    /// One `P6` frame: header plus `width * height * 3` payload bytes.
    fn ppm_frame(width: usize, height: usize, fill: u8) -> Vec<u8> {
        let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
        out.extend(std::iter::repeat_n(fill, width * height * 3));
        out
    }

    #[test]
    fn header_reads_the_geometry_ffmpeg_emits() {
        assert_eq!(header(b"P6\n1920 1080\n255\n"), Some((1920, 1080)));
    }

    #[test]
    fn header_accepts_space_separated_and_commented_variants() {
        // PPM only requires whitespace between tokens, not newlines.
        assert_eq!(header(b"P6 640 480 255 "), Some((640, 480)));
        assert_eq!(
            header(b"P6\n# written by ffmpeg\n640 480\n255\n"),
            Some((640, 480))
        );
    }

    #[test]
    fn header_rejects_sixteen_bit_samples() {
        // maxval > 255 doubles the payload stride; decoding it as 8-bit would
        // silently tear every frame.
        assert_eq!(header(b"P6\n64 64\n65535\n"), None);
    }

    #[test]
    fn header_rejects_wrong_magic_and_degenerate_geometry() {
        assert_eq!(header(b"P5\n64 64\n255\n"), None, "P5 is greyscale");
        assert_eq!(header(b"P6\n0 64\n255\n"), None, "zero width");
        assert_eq!(header(b"P6\n64 0\n255\n"), None, "zero height");
        assert_eq!(header(b"P6\nwide 64\n255\n"), None, "non-numeric");
        assert_eq!(header(b"P6\n64 64\n"), None, "truncated header");
    }

    #[test]
    fn header_rejects_geometry_beyond_the_frame_cap() {
        // Trust boundary: the header comes off a pipe, so an absurd geometry
        // must be refused rather than turned into an allocation.
        let over = format!("P6\n{MAX_FRAME_BYTES} {MAX_FRAME_BYTES}\n255\n");
        assert_eq!(header(over.as_bytes()), None);
        let overflow = format!("P6\n{} {}\n255\n", usize::MAX, usize::MAX);
        assert_eq!(header(overflow.as_bytes()), None, "must not overflow");
    }

    #[test]
    fn consecutive_frames_are_published_in_order() {
        let shared = test_shared();
        let mut stream = ppm_frame(2, 2, 0x11);
        stream.extend(ppm_frame(2, 2, 0x22));
        read_frames(&shared, Cursor::new(stream));

        let frame = shared
            .frame
            .lock()
            .expect("frame lock")
            .clone()
            .expect("a frame");
        assert_eq!(frame.generation, 2, "both frames must be consumed");
        assert_eq!(frame.rgb, vec![0x22; 12], "latest frame wins");
        assert_eq!(*shared.status.lock().expect("status lock"), Status::Online);
    }

    #[test]
    fn mid_stream_resolution_change_is_followed() {
        // The whole point of per-frame headers: the old code fixed geometry
        // once from ffprobe and tore every frame after a resolution change.
        let shared = test_shared();
        let mut stream = ppm_frame(2, 2, 0x11);
        stream.extend(ppm_frame(4, 1, 0x33));
        read_frames(&shared, Cursor::new(stream));

        let frame = shared
            .frame
            .lock()
            .expect("frame lock")
            .clone()
            .expect("a frame");
        assert_eq!((frame.width, frame.height), (4, 1));
        assert_eq!(frame.rgb.len(), 12);
    }

    #[test]
    fn truncated_payload_stops_without_publishing_a_partial_frame() {
        let shared = test_shared();
        let mut stream = ppm_frame(2, 2, 0x11);
        stream.truncate(stream.len() - 3); // lose one pixel
        read_frames(&shared, Cursor::new(stream));
        assert!(
            shared.frame.lock().expect("frame lock").is_none(),
            "a short read must publish nothing"
        );
    }

    #[test]
    fn shutdown_stops_the_reader_before_the_next_frame() {
        let shared = test_shared();
        shared.shutdown.store(true, Ordering::Relaxed);
        read_frames(&shared, Cursor::new(ppm_frame(2, 2, 0x11)));
        assert!(shared.frame.lock().expect("frame lock").is_none());
    }

    #[test]
    fn handle_is_constructible_without_ffmpeg_present() {
        // Guards the public surface used by app.rs; the worker thread failing
        // to spawn ffmpeg must not panic the caller.
        let handle = StreamHandle::spawn("rtsp://127.0.0.1:1/none", true);
        handle.stop();
    }

    #[test]
    fn concurrent_recording_starts_publish_at_most_one_child() {
        let ffmpeg_available = std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok();
        if !ffmpeg_available {
            eprintln!("ffmpeg not available, skipping");
            return;
        }

        let handle = std::sync::Arc::new(StreamHandle::spawn("rtsp://127.0.0.1:1/none", true));
        let callers = (0..8)
            .map(|_| {
                let handle = std::sync::Arc::clone(&handle);
                std::thread::spawn(move || handle.start_recording("/dev/full"))
            })
            .collect::<Vec<_>>();
        for caller in callers {
            caller
                .join()
                .expect("recording caller finishes")
                .expect("start is idempotent");
        }
        assert!(
            handle
                .shared
                .recorder
                .lock()
                .expect("recorder lock")
                .is_some()
        );
        handle.stop_recording();
        let deadline = Instant::now() + Duration::from_secs(10);
        while matches!(
            handle.recording_state(),
            super::RecordingState::Saving { .. }
        ) && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !matches!(
                handle.recording_state(),
                super::RecordingState::Recording { .. }
            ),
            "stop must not leave the state Recording"
        );
        handle.stop();
    }

    #[test]
    fn a_frame_resets_the_attempt_count_and_marks_has_frame() {
        let shared = test_shared();
        shared.attempts.store(3, Ordering::Relaxed);
        assert!(!shared.has_frame.load(Ordering::Relaxed));

        read_frames(&shared, Cursor::new(ppm_frame(2, 2, 0x11)));

        assert_eq!(shared.attempts.load(Ordering::Relaxed), 0);
        assert!(shared.has_frame.load(Ordering::Relaxed));
    }

    #[test]
    fn begin_attempt_increments_attempts_and_clears_the_retry_deadline() {
        let shared = test_shared();
        *shared.next_retry.lock().expect("lock") = Some(Instant::now());

        begin_attempt(&shared);
        assert_eq!(shared.attempts.load(Ordering::Relaxed), 1);
        assert!(shared.next_retry.lock().expect("lock").is_none());
        assert_eq!(*shared.status.lock().expect("lock"), Status::Connecting);

        begin_attempt(&shared);
        assert_eq!(
            shared.attempts.load(Ordering::Relaxed),
            2,
            "each failed attempt keeps counting up"
        );
    }

    #[test]
    fn spawning_the_process_does_not_promote_status_to_online() {
        // R3-online-on-spawn: a spawned ffmpeg process only proves the local
        // binary launched, not that the camera is reachable. Status must
        // stay Connecting (as `begin_attempt` set it) until a frame arrives.
        let shared = test_shared();
        begin_attempt(&shared);

        on_process_spawned(&shared);

        assert_eq!(
            *shared.status.lock().expect("lock"),
            Status::Connecting,
            "spawn alone must not promote status to Online"
        );
    }

    #[test]
    fn begin_attempt_clears_has_frame_from_a_previous_attempt() {
        // R3-online-on-spawn: a frame published by a prior attempt must not
        // make the next reconnect attempt look reachable before it proves so.
        let shared = test_shared();
        shared.has_frame.store(true, Ordering::Relaxed);

        begin_attempt(&shared);

        assert!(
            !shared.has_frame.load(Ordering::Relaxed),
            "has_frame must reset at the start of a new attempt"
        );
    }

    #[test]
    fn enter_backoff_schedules_a_retry_deadline_and_waits_it_out() {
        let shared = test_shared();
        let delay = Duration::from_millis(150);
        let started = Instant::now();
        enter_backoff(&shared, delay);
        let elapsed = started.elapsed();

        assert!(
            elapsed >= delay,
            "must wait out the full delay absent a wake: {elapsed:?}"
        );
        assert_eq!(*shared.status.lock().expect("lock"), Status::Offline);
        assert!(
            shared.next_retry.lock().expect("lock").is_none(),
            "deadline is cleared once the backoff ends"
        );
    }

    #[test]
    fn retry_wake_shortens_an_in_progress_backoff() {
        let shared = test_shared();
        let long_delay = Duration::from_secs(5);
        let retry_wake = Arc::clone(&shared.retry_wake);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            retry_wake.store(true, Ordering::Relaxed);
        });

        let started = Instant::now();
        enter_backoff(&shared, long_delay);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "retry_wake must cut the 5s backoff short, took {elapsed:?}"
        );
    }

    #[test]
    fn sleep_interruptible_consumes_the_wake_flag_so_it_never_leaks_into_a_later_sleep() {
        let shutdown = AtomicBool::new(false);
        let paused = AtomicBool::new(false);
        let retry_wake = AtomicBool::new(true);

        // First call must return almost immediately because the flag is
        // already set...
        let started = Instant::now();
        sleep_interruptible(&shutdown, &paused, &retry_wake, Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));

        // ...and the flag must be cleared afterward, or a completely
        // unrelated later sleep (e.g. the next reconnect delay) would also
        // be cut short by a stale wake nobody asked for.
        assert!(!retry_wake.load(Ordering::Relaxed));
        let started = Instant::now();
        sleep_interruptible(&shutdown, &paused, &retry_wake, Duration::from_millis(100));
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn stream_handle_exposes_attempts_retry_deadline_and_has_frame() {
        // Guards the public surface app.rs reads for tiles: constructible
        // and readable without ffmpeg being present.
        let handle = StreamHandle::spawn("rtsp://127.0.0.1:1/none", true);
        let _ = handle.attempts();
        let _ = handle.next_retry_at();
        let _ = handle.phase_started_at();
        assert!(!handle.has_frame());
        handle.retry_now();
        handle.stop();
    }

    #[test]
    fn recording_lifecycle_is_observable_and_stop_is_idempotent() {
        let handle = StreamHandle::spawn("rtsp://127.0.0.1:1/none", true);
        assert_eq!(handle.recording_state(), super::RecordingState::Idle);
        let result = handle.start_recording("/dev/full");
        assert!(
            result.is_ok() || matches!(handle.recording_state(), super::RecordingState::Failed(_))
        );
        assert!(matches!(
            handle.recording_state(),
            super::RecordingState::Recording { .. } | super::RecordingState::Failed(_)
        ));
        handle.stop_recording();
        let deadline = Instant::now() + Duration::from_secs(10);
        while matches!(
            handle.recording_state(),
            super::RecordingState::Saving { .. }
        ) && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !matches!(
                handle.recording_state(),
                super::RecordingState::Recording { .. }
            ),
            "stop must not leave the state Recording"
        );
        handle.stop();
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

#[cfg(test)]
mod recording_tests {
    use super::{
        RECORDING_STOP_TIMEOUT, RecordingState, StreamHandle, finish_recording, lock,
        recording_args, settle_exit, spawn_recorder,
    };

    #[test]
    fn a_recording_that_cannot_start_reports_why() {
        let handle = StreamHandle::spawn("rtsp://127.0.0.1:1/none", true);
        handle.report_recording_error("creating /x: permission denied".to_owned());
        assert_eq!(
            handle.recording_state(),
            RecordingState::Failed("creating /x: permission denied".to_owned())
        );
        handle.stop();
    }
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cam-viewer-rec-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Binds a loopback listener that accepts one connection and then goes
    /// silent (never sends data): a deterministic stand-in for a camera
    /// whose input stalls mid-read (S1's stalled-input bug). A plain FIFO
    /// does not reproduce this: blocking open() on it is not interrupted by
    /// SIGINT (confirmed against this system's ffmpeg build — the open()
    /// auto-restarts), whereas a stalled network read goes through ffmpeg's
    /// protocol layer, which does check its interrupt callback and is
    /// exactly what a stalled RTSP camera connection looks like.
    fn black_hole_listener() -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            if let Ok((_stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(30));
            }
        });
        addr
    }

    /// Args for an ffmpeg recording a stalled `black_hole_listener` address.
    fn stalled_input_args(addr: std::net::SocketAddr, out: &Path) -> Vec<String> {
        [
            "-v".to_owned(),
            "error".to_owned(),
            "-i".to_owned(),
            format!("tcp://{addr}"),
            "-c".to_owned(),
            "copy".to_owned(),
            "-f".to_owned(),
            "mp4".to_owned(),
            out.display().to_string(),
        ]
        .to_vec()
    }

    #[test]
    fn recordings_keep_audio_whatever_the_mute_state() {
        let args = recording_args("rtsp://cam/live", Path::new("/tmp/out.mp4"));
        let joined = args.join(" ");
        assert!(joined.contains("-map 0:v:0 -map 0:a?"), "{joined}");
        // Cameras often send G.711, which MP4 cannot carry as-is.
        assert!(joined.contains("-c:v copy -c:a aac"), "{joined}");
    }

    #[test]
    fn recordings_never_overwrite_and_never_wait_forever_on_input() {
        let args = recording_args("rtsp://cam/live", Path::new("/tmp/out.mp4"));
        assert!(args.iter().any(|a| a == "-n"), "{args:?}");
        assert!(args.iter().any(|a| a == "-timeout"), "{args:?}");
        // stdin carries the `q` that finishes the file cleanly.
        assert!(!args.iter().any(|a| a == "-nostdin"), "{args:?}");
        assert_eq!(args.last().map(String::as_str), Some("/tmp/out.mp4"));
    }

    #[test]
    fn an_exit_nobody_asked_for_is_a_failure_even_with_status_zero() {
        // ffmpeg exits 0 after "File 'x' already exists. Exiting."
        let state = settle_exit(
            Path::new("/nope.mp4"),
            true,
            false,
            "File 'x' already exists. Exiting.",
        );
        assert_eq!(
            state,
            RecordingState::Failed("File 'x' already exists. Exiting.".to_owned())
        );
        let state = settle_exit(Path::new("/nope.mp4"), false, false, "");
        assert_eq!(
            state,
            RecordingState::Failed("recording stopped unexpectedly".to_owned())
        );
    }

    #[test]
    fn a_requested_stop_with_no_file_reports_that_nothing_was_recorded() {
        let state = settle_exit(Path::new("/definitely/missing.mp4"), true, true, "");
        assert_eq!(
            state,
            RecordingState::Failed("nothing was recorded".to_owned())
        );
    }

    #[test]
    fn a_requested_stop_with_a_file_is_saved() {
        let dir = scratch("saved");
        let path = dir.join("ok.mp4");
        std::fs::write(&path, b"data").expect("file");
        assert!(matches!(
            settle_exit(&path, true, true, ""),
            RecordingState::Saved { path: saved, .. } if saved == path
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finishing_a_recording_leaves_a_file_ffprobe_can_read() {
        if !ffmpeg_available() {
            eprintln!("ffmpeg not available, skipping");
            return;
        }
        let dir = scratch("finish");
        let path = dir.join("clip.mp4");
        let args: Vec<String> = [
            "-v",
            "error",
            "-n",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=15",
            "-c:v",
            "libx264",
            "-f",
            "mp4",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .chain([path.display().to_string()])
        .collect();
        let recorder = spawn_recorder(&args, path.clone()).expect("spawn ffmpeg");
        std::thread::sleep(Duration::from_millis(1500));
        let state = finish_recording(recorder, Duration::from_secs(5));
        assert!(matches!(state, RecordingState::Saved { .. }), "{state:?}");
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type",
                "-of",
                "csv=p=0",
            ])
            .arg(&path)
            .output()
            .expect("run ffprobe");
        assert!(
            probe.status.success(),
            "{}",
            String::from_utf8_lossy(&probe.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "video");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_recording_settles_an_already_exited_child_as_failed_not_saved() {
        // Defect 4 (S2): an ffmpeg that already exited on its own (bounded
        // -t clip) must not be read as a requested stop just because
        // `stop_recording` happened to be called after the fact.
        if !ffmpeg_available() {
            eprintln!("ffmpeg not available, skipping");
            return;
        }
        let dir = scratch("already-exited");
        let path = dir.join("clip.mp4");
        let args: Vec<String> = [
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=15",
            "-t",
            "0.3",
            "-c:v",
            "libx264",
            "-f",
            "mp4",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .chain([path.display().to_string()])
        .collect();
        let recorder = spawn_recorder(&args, path.clone()).expect("spawn ffmpeg");

        let handle = StreamHandle::spawn("rtsp://127.0.0.1:1/none", true);
        {
            let mut slot = lock(&handle.shared.recorder);
            *slot = Some(recorder);
            let rec = slot.as_mut().expect("just inserted");
            let deadline = Instant::now() + Duration::from_secs(5);
            let exited = loop {
                if matches!(rec.child.try_wait(), Ok(Some(_))) {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            assert!(
                exited,
                "the bounded clip must finish on its own well within 5s"
            );
        }
        *lock(&handle.shared.recording_state) = RecordingState::Recording {
            path: path.clone(),
            started: Instant::now(),
        };

        handle.stop_recording();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = handle.recording_state();
        while matches!(state, RecordingState::Saving { .. }) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            state = handle.recording_state();
        }

        assert!(
            matches!(state, RecordingState::Failed(_)),
            "an ffmpeg that already exited before STOP must settle as Failed, not Saved: {state:?}"
        );
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn finish_recording_escalates_to_sigint_on_a_stalled_input_instead_of_waiting_the_full_timeout()
    {
        // Defect 3 (S1): a stalled input (a camera connection accepted
        // then gone silent) never gets data, so ffmpeg blocks on the read
        // and never reaches the point of reading `q` from stdin. Only a
        // signal can unblock it.
        if !ffmpeg_available() {
            eprintln!("ffmpeg not available, skipping");
            return;
        }
        let dir = scratch("stalled-finish");
        let addr = black_hole_listener();
        let path = dir.join("clip.mp4");
        let recorder =
            spawn_recorder(&stalled_input_args(addr, &path), path.clone()).expect("spawn ffmpeg");

        let timeout = Duration::from_secs(5);
        let start = Instant::now();
        let state = finish_recording(recorder, timeout);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(4),
            "SIGINT around the 2s mark must unblock a stalled read well before the {timeout:?} \
             SIGKILL fallback, took {elapsed:?}"
        );
        assert!(
            matches!(&state, RecordingState::Failed(msg) if !msg.contains("force-stopped")),
            "a SIGINT-unblocked stop must not read as a forced-timeout SIGKILL stop: {state:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_handle_with_a_stalled_recorder_never_blocks_the_caller() {
        // Defect 2 (S1/S2): removing/editing a camera via `apply_cameras`
        // drops its `StreamHandle` on the UI thread. That must never wait
        // out a recording's stop timeout.
        if !ffmpeg_available() {
            eprintln!("ffmpeg not available, skipping");
            return;
        }
        let dir = scratch("drop-no-block");
        let addr = black_hole_listener();
        let path = dir.join("clip.mp4");
        let recorder =
            spawn_recorder(&stalled_input_args(addr, &path), path.clone()).expect("spawn ffmpeg");

        let handle = StreamHandle::spawn("rtsp://127.0.0.1:1/none", true);
        *lock(&handle.shared.recorder) = Some(recorder);
        *lock(&handle.shared.recording_state) = RecordingState::Recording {
            path: path.clone(),
            started: Instant::now(),
        };

        let start = Instant::now();
        drop(handle);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "dropping a handle with an active recorder must never block the caller \
             (camera removal/edit runs on the UI thread): took {elapsed:?}"
        );
        // The background finish is still in flight; give it a moment before
        // cleaning up so it does not race the directory removal.
        std::thread::sleep(Duration::from_secs(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn signalling_then_joining_multiple_stalled_recordings_waits_in_parallel_not_sequentially() {
        // Defect 5a (app exit): signal every camera first, then wait for
        // them together — not one camera's full timeout after another.
        if !ffmpeg_available() {
            eprintln!("ffmpeg not available, skipping");
            return;
        }
        const COUNT: usize = 3;
        let mut handles = Vec::new();
        let mut dirs = Vec::new();
        for i in 0..COUNT {
            let dir = scratch(&format!("parallel-signal-{i}"));
            let addr = black_hole_listener();
            let path = dir.join("clip.mp4");
            let recorder = spawn_recorder(&stalled_input_args(addr, &path), path.clone())
                .expect("spawn ffmpeg");

            let handle = StreamHandle::spawn("rtsp://127.0.0.1:1/none", true);
            *lock(&handle.shared.recorder) = Some(recorder);
            *lock(&handle.shared.recording_state) = RecordingState::Recording {
                path: path.clone(),
                started: Instant::now(),
            };
            handles.push(handle);
            dirs.push(dir);
        }

        let start = Instant::now();
        for handle in &handles {
            handle.stop();
        }
        for handle in &handles {
            handle.join_finish();
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed < RECORDING_STOP_TIMEOUT * 2,
            "signalling all {COUNT} cameras before waiting must not cost {COUNT} * \
             the per-camera stop timeout ({RECORDING_STOP_TIMEOUT:?}), took {elapsed:?}"
        );

        for dir in dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
