use std::io::Read;
use std::process::{Command, Stdio};

const WIDTH: usize = 320;
const HEIGHT: usize = 240;

#[test]
fn ffmpeg_rawvideo_pipe_produces_frames() {
    let ok_ffmpeg = Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !ok_ffmpeg {
        eprintln!("ffmpeg not available, skipping");
        return;
    }

    let mut child = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={WIDTH}x{HEIGHT}:rate=15"),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn ffmpeg");

    let expected_len = WIDTH * HEIGHT * 3;
    let mut buf = vec![0u8; expected_len];
    let mut stdout = child.stdout.take().expect("stdout piped");
    stdout
        .read_exact(&mut buf)
        .expect("failed to read one full frame from ffmpeg");

    let _ = child.wait();

    let non_black = buf.iter().any(|&b| b > 16);
    assert!(
        non_black,
        "frame is entirely black; rawvideo pipe did not carry pixel data"
    );
}
