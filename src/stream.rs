use anyhow::{Context, Result};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const PAUSED_PROBE_INTERVAL: Duration = Duration::from_secs(8);
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

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
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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

fn probe(url: &str) -> Result<(usize, usize)> {
    let output = Command::new("ffprobe")
        .args([
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
            url,
        ])
        .output()
        .context("spawning ffprobe")?;
    if !output.status.success() {
        anyhow::bail!("ffprobe exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next().unwrap_or("").trim();
    let (w, h) = line
        .split_once('x')
        .context("unexpected ffprobe output format")?;
    let width: usize = w.parse().context("parsing width")?;
    let height: usize = h.parse().context("parsing height")?;
    Ok((width, height))
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
            let _ = probe(url);
            sleep_interruptible(
                &shared.shutdown,
                &shared.paused,
                PAUSED_PROBE_INTERVAL,
            );
            continue;
        }
        paused_seen = false;
        *lock(&shared.status) = Status::Connecting;

        let dims = match probe(url) {
            Ok(dims) if dims.0 > 0 && dims.1 > 0 => dims,
            Ok(_) | Err(_) => {
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
