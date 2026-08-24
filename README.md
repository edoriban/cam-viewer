# cam-viewer

A multi-camera RTSP viewer for desktop, written in Rust with [egui](https://github.com/emilk/egui) ([eframe](https://github.com/emilk/egui/tree/master/crates/eframe)).

cam-viewer shows live video from IP cameras that expose an RTSP stream. Each camera is decoded by its own `ffmpeg` subprocess, which pipes raw RGB frames into the app over a pipe — no heavyweight media frameworks are linked in.

## Features

- **Solo and grid views** — click a tile to view a single camera full-window; return to the grid at any time.
- **Auto-pause on focus loss** — streams pause when the window loses focus (and resume when it regains focus), saving bandwidth and CPU.
- **In-app settings editor** — add, rename, edit, or remove cameras without touching a config file.
- **Per-camera ffmpeg decoding** — one `ffmpeg` process per camera decodes the RTSP stream and outputs rawvideo frames.
- **Automatic reconnection** — offline or failing streams are retried with a backoff and clearly marked as connecting/online/offline/paused.
- **Single-owner threads** — each stream has one owner thread that manages the ffmpeg child process lifecycle.

## How it works

- The app reads its configuration from a TOML file (see below).
- For every configured camera, a dedicated thread spawns an `ffmpeg` subprocess:

  ```
  ffmpeg -rtsp_transport tcp -i <url> -f rawvideo -pix_fmt rgb24 - <pipe>
  ```

  Frames are read from the pipe into shared state (`Arc<Mutex<...>>`) and uploaded as GPU textures by the UI.
- When the window is unfocused for a while, all streams are paused: ffmpeg children are stopped and only probed periodically until focus returns.
- Crashed or exited ffmpeg processes are restarted automatically after a short delay.

## Install / build

Requirements:

- Rust (stable, edition 2024)
- `ffmpeg` (and optionally `ffprobe`) available in `PATH` at runtime

```sh
cargo build --release
./target/release/cam-viewer
```

Linux is supported (X11/Wayland via eframe). Windows builds are provided as release assets.

## Configuration

Configuration lives in `~/.config/cam-viewer/cameras.toml` (`$XDG_CONFIG_HOME/cam-viewer/cameras.toml` if set). The file is created automatically on first run and can also be edited from inside the app.

Example:

```toml
[[cameras]]
name = "Front door"
url = "rtsp://CAMERA_IP:554/live/ch0"

[[cameras]]
name = "Backyard"
url = "rtsp://CAMERA_IP:554/stream1"
```

Each camera needs a `name` and an RTSP `url`.

### Finding your camera's RTSP URL

RTSP paths vary by vendor. Common patterns include `/live/ch0`, `/stream1`, `/h264`, or `/cam/realmonitor?channel=1&subtype=0`. Check your camera's manual or web UI, or probe with `ffprobe "rtsp://CAMERA_IP:554/live/ch0"`.

## License

Copyright (c) 2026 edoriban

Licensed under the [PolyForm Noncommercial 1.0.0](LICENSE). Free for personal and other noncommercial use only; commercial use requires a separate license.
