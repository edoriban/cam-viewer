use std::io::{BufRead, BufReader, Read};
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
    Recording,
    Failed(String),
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
    recording: Arc<Mutex<Option<Child>>>,
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
            recording: Arc::new(Mutex::new(None)),
            recording_state: Arc::new(Mutex::new(RecordingState::Idle)),
            audio_child: Arc::new(Mutex::new(None)),
            attempts: Arc::new(AtomicU32::new(0)),
            next_retry: Arc::new(Mutex::new(None)),
            phase_started: Arc::new(Mutex::new(Instant::now())),
            has_frame: Arc::new(AtomicBool::new(false)),
            retry_wake: Arc::new(AtomicBool::new(false)),
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

    pub fn stop(&self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.stop_recording();
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

    pub fn recording_state(&self) -> RecordingState {
        let mut child_slot = lock(&self.shared.recording);
        if let Some(child) = child_slot.as_mut() {
            match child.try_wait() {
                Ok(None) => {}
                Ok(Some(status)) => {
                    child_slot.take();
                    let mut state = lock(&self.shared.recording_state);
                    *state = if status.success() {
                        RecordingState::Idle
                    } else {
                        RecordingState::Failed("ffmpeg recording exited with an error".to_owned())
                    };
                }
                Err(error) => {
                    child_slot.take();
                    *lock(&self.shared.recording_state) =
                        RecordingState::Failed(format!("checking recording process: {error}"));
                }
            }
        }
        lock(&self.shared.recording_state).clone()
    }

    pub fn start_recording(&self, path: impl Into<String>) -> Result<(), String> {
        // Keep the duplicate check, process creation, and child publication
        // under one mutex. Otherwise two callers can both observe Idle and
        // spawn separate ffmpeg children before either publishes its child.
        let mut recording = lock(&self.shared.recording);
        if let Some(child) = recording.as_mut() {
            match child.try_wait() {
                Ok(None) => {
                    *lock(&self.shared.recording_state) = RecordingState::Recording;
                    return Ok(());
                }
                Ok(Some(status)) => {
                    recording.take();
                    *lock(&self.shared.recording_state) = if status.success() {
                        RecordingState::Idle
                    } else {
                        RecordingState::Failed("ffmpeg recording exited with an error".to_owned())
                    };
                }
                Err(error) => {
                    recording.take();
                    *lock(&self.shared.recording_state) =
                        RecordingState::Failed(format!("checking recording process: {error}"));
                }
            }
        }

        let path = path.into();
        let muted = self.shared.muted.load(Ordering::Relaxed);
        let mut command = no_window_command("ffmpeg");
        command.args(["-nostdin", "-v", "error", "-rtsp_transport", "tcp"]);
        command.args(["-i", &self.shared.url, "-map", "0:v:0"]);
        if !muted {
            command.args(["-map", "0:a?"]);
        }
        command.args(["-c", "copy", "-f", "mp4", &path]);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = format!("starting recording: {error}");
                *lock(&self.shared.recording_state) = RecordingState::Failed(message.clone());
                return Err(message);
            }
        };
        *recording = Some(child);
        *lock(&self.shared.recording_state) = RecordingState::Recording;
        Ok(())
    }

    pub fn stop_recording(&self) {
        stop_child(&self.shared.recording);
        *lock(&self.shared.recording_state) = RecordingState::Idle;
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
/// default output device. Gated by the same `muted` flag the recording
/// pipeline already reads, so unmuting a camera does both at once.
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
            recording: Arc::new(Mutex::new(None)),
            recording_state: Arc::new(Mutex::new(super::RecordingState::Idle)),
            audio_child: Arc::new(Mutex::new(None)),
            attempts: Arc::new(AtomicU32::new(0)),
            next_retry: Arc::new(Mutex::new(None)),
            phase_started: Arc::new(Mutex::new(Instant::now())),
            has_frame: Arc::new(AtomicBool::new(false)),
            retry_wake: Arc::new(AtomicBool::new(false)),
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
                .recording
                .lock()
                .expect("recording lock")
                .is_some()
        );
        handle.stop_recording();
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
            super::RecordingState::Recording | super::RecordingState::Failed(_)
        ));
        handle.stop_recording();
        assert_eq!(handle.recording_state(), super::RecordingState::Idle);
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
