//! Best-effort "a newer release exists" check against the GitHub Releases API.
//!
//! Deliberately passive: it never downloads, replaces, or executes anything.
//! The app only learns that a newer tag was published and shows a link. Users
//! who found this build through a zip have no other way to hear about a fix,
//! which is the whole reason the check exists.
//!
//! Every failure is silent. A camera viewer must start and show video on a
//! network with no internet, behind a proxy, or while GitHub is down, so the
//! check runs on its own thread and simply produces nothing when it cannot
//! reach the API.

use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Public releases endpoint for this repository. Anonymous requests are rate
/// limited per IP (60/hour at time of writing), which one check per app start
/// stays far below.
const RELEASES_URL: &str = "https://api.github.com/repos/edoriban/cam-viewer/releases/latest";

/// GitHub rejects API requests that send no User-Agent.
const USER_AGENT: &str = concat!("cam-viewer/", env!("CARGO_PKG_VERSION"));

/// Whole-request deadline. The check is worthless if it can hang a thread for
/// minutes against a black-holed proxy.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    html_url: String,
}

/// A published release strictly newer than the running build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    /// Version as published, without any leading `v`.
    pub version: String,
    /// Release page to open; never a direct asset download.
    pub url: String,
}

/// Handle the UI polls once per repaint. `None` means "nothing to report",
/// which is also what every failure looks like.
pub type Shared = Arc<Mutex<Option<Available>>>;

/// Parses `0.4.0` or `v0.4.0` into comparable parts. Anything with a
/// pre-release suffix or a non-numeric component is rejected rather than
/// guessed at, so a tag scheme change can never invent an update.
fn parse_version(raw: &str) -> Option<(u32, u32, u32)> {
    let trimmed = raw.trim().trim_start_matches('v');
    let mut parts = trimmed.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Whether `latest` is strictly newer than `current`. Unparsable input on
/// either side means "no update", never a prompt.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// Queries the API once and reports a strictly newer release. `None` on any
/// transport, status, or parse failure.
fn fetch(current: &str) -> Option<Available> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .user_agent(USER_AGENT)
        .build()
        .into();
    let release: ReleaseResponse = agent
        .get(RELEASES_URL)
        .call()
        .ok()?
        .body_mut()
        .read_json()
        .ok()?;
    if !is_newer(&release.tag_name, current) {
        return None;
    }
    Some(Available {
        version: release.tag_name.trim_start_matches('v').to_owned(),
        url: release.html_url,
    })
}

/// Runs one check on a detached thread and returns the handle the UI reads.
///
/// The thread is never joined: it holds only an `Arc` and dies on its own, so
/// a slow API call can never delay application shutdown.
pub fn spawn_check() -> Shared {
    let shared: Shared = Arc::new(Mutex::new(None));
    let writer = Arc::clone(&shared);
    let current = env!("CARGO_PKG_VERSION");
    let spawned = thread::Builder::new()
        .name("update-check".to_owned())
        .spawn(move || {
            if let Some(found) = fetch(current)
                && let Ok(mut slot) = writer.lock()
            {
                *slot = Some(found);
            }
        });
    // A thread that refuses to spawn is not worth surfacing; the app simply
    // never learns about an update.
    let _ = spawned;
    shared
}

#[cfg(test)]
mod tests {
    use super::{is_newer, parse_version};

    #[test]
    fn parses_tags_with_and_without_the_v_prefix() {
        assert_eq!(parse_version("0.4.0"), Some((0, 4, 0)));
        assert_eq!(parse_version("v0.4.0"), Some((0, 4, 0)));
        assert_eq!(parse_version(" v1.20.3 "), Some((1, 20, 3)));
    }

    #[test]
    fn rejects_shapes_it_cannot_compare() {
        for raw in ["", "v", "1.2", "1.2.3.4", "1.2.x", "0.5.0-rc.1", "latest"] {
            assert_eq!(parse_version(raw), None, "must reject {raw:?}");
        }
    }

    #[test]
    fn detects_a_strictly_newer_release() {
        assert!(is_newer("v0.5.0", "0.4.0"));
        assert!(is_newer("v0.4.1", "0.4.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
    }

    #[test]
    fn same_or_older_release_is_not_an_update() {
        assert!(!is_newer("v0.4.0", "0.4.0"), "same version");
        assert!(!is_newer("v0.3.9", "0.4.0"), "older tag");
        assert!(!is_newer("v0.4.0", "0.4.1"), "running a newer local build");
    }

    #[test]
    fn numeric_components_compare_as_numbers_not_strings() {
        // "10" sorts before "9" as text; the running build must not be told
        // it is ahead of a release it is behind.
        assert!(is_newer("v0.10.0", "0.9.0"));
        assert!(!is_newer("v0.9.0", "0.10.0"));
    }

    #[test]
    fn unparsable_input_never_prompts_an_update() {
        assert!(!is_newer("nightly", "0.4.0"));
        assert!(!is_newer("v0.5.0", "dev"));
        assert!(!is_newer("v0.5.0-beta", "0.4.0"), "pre-release is not a bump");
    }
}
