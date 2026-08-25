//! Live validation on developer hardware only (task 7.3 — optional,
//! non-blocking). Runs against a real camera URL:
//!
//! ```text
//! CAM_VIEWER_TEST_RTSP_URL="rtsp://user:pass@192.168.1.64:554/stream" \
//!     cargo test --test live_probe -- --ignored
//! ```

use cam_viewer::stream::{ProbeOutcome, probe_rtsp};
use std::time::Duration;

#[test]
#[ignore = "live hardware only: set CAM_VIEWER_TEST_RTSP_URL to a reachable RTSP URL"]
fn live_rtsp_url_probes_success_with_dimensions() {
    let Ok(url) = std::env::var("CAM_VIEWER_TEST_RTSP_URL") else {
        eprintln!("CAM_VIEWER_TEST_RTSP_URL not set; skipping");
        return;
    };
    match probe_rtsp(&url, Duration::from_secs(6)) {
        ProbeOutcome::Success { width, height } => {
            assert!(width > 0 && height > 0, "dimensions must be positive");
        }
        other => panic!("expected Success with dimensions, got {other:?}"),
    }
}
