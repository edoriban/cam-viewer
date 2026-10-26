//! Where recordings are written and what they are called.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SUBDIR: &str = "cam-viewer";

/// Folder recordings go to: `$XDG_VIDEOS_DIR/cam-viewer` when set, else
/// `~/Videos/cam-viewer`. Not created here; see [`prepare`].
pub fn directory() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    directory_from(std::env::var_os("XDG_VIDEOS_DIR"), home)
}

fn directory_from(videos: Option<OsString>, home: Option<OsString>) -> PathBuf {
    // The XDG spec says relative values must be ignored.
    if let Some(videos) = videos.map(PathBuf::from).filter(|p| p.is_absolute()) {
        return videos.join(SUBDIR);
    }
    match home {
        Some(home) => PathBuf::from(home).join("Videos").join(SUBDIR),
        None => PathBuf::from("cam-viewer-recordings"),
    }
}

/// Creates the recordings folder and returns a path for a new recording of
/// `camera` that no existing file occupies.
pub fn prepare(camera: &str) -> Result<PathBuf, String> {
    let dir = directory();
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("creating {}: {error}", dir.display()))?;
    Ok(unique_path(&dir, &file_name(camera, SystemTime::now())))
}

/// `<camera>_<YYYY-MM-DD>T<HH-MM-SS>Z.mp4`, in UTC so the name is correct
/// without a time zone database.
fn file_name(camera: &str, at: SystemTime) -> String {
    let safe: String = camera
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{safe}_{year:04}-{month:02}-{day:02}T{:02}-{:02}-{:02}Z.mp4",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-to-civil conversion for the proleptic Gregorian
/// calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `dir/name`, or `dir/<stem>-N.<ext>` with the first free N from 2.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    (2..)
        .map(|n| {
            if ext.is_empty() {
                dir.join(format!("{stem}-{n}"))
            } else {
                dir.join(format!("{stem}-{n}.{ext}"))
            }
        })
        .find(|candidate| !candidate.exists())
        .unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn file_name_carries_the_camera_and_a_utc_timestamp() {
        // 2026-10-26T23:42:08Z
        let at = UNIX_EPOCH + Duration::from_secs(1_793_058_128);
        assert_eq!(file_name("Patio", at), "Patio_2026-10-26T23-42-08Z.mp4");
    }

    #[test]
    fn file_name_replaces_characters_that_are_unsafe_in_paths() {
        let at = UNIX_EPOCH;
        assert_eq!(
            file_name("Entrada / principal:1", at),
            "Entrada___principal_1_1970-01-01T00-00-00Z.mp4"
        );
    }

    #[test]
    fn unique_path_never_reuses_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("cam-viewer-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let first = unique_path(&dir, "Patio_x.mp4");
        assert_eq!(first, dir.join("Patio_x.mp4"));
        std::fs::write(&first, b"").expect("occupy first");
        assert_eq!(unique_path(&dir, "Patio_x.mp4"), dir.join("Patio_x-2.mp4"));
        std::fs::write(dir.join("Patio_x-2.mp4"), b"").expect("occupy second");
        assert_eq!(unique_path(&dir, "Patio_x.mp4"), dir.join("Patio_x-3.mp4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_prefers_the_xdg_videos_dir() {
        assert_eq!(
            directory_from(
                Some(OsString::from("/media/vids")),
                Some(OsString::from("/home/u"))
            ),
            PathBuf::from("/media/vids/cam-viewer")
        );
    }

    #[test]
    fn directory_falls_back_to_videos_under_home() {
        assert_eq!(
            directory_from(None, Some(OsString::from("/home/u"))),
            Path::new("/home/u").join("Videos").join("cam-viewer")
        );
        // A relative XDG value is ignored, as the XDG spec requires.
        assert_eq!(
            directory_from(
                Some(OsString::from("vids")),
                Some(OsString::from("/home/u"))
            ),
            Path::new("/home/u").join("Videos").join("cam-viewer")
        );
    }

    #[test]
    fn directory_without_home_stays_relative_but_named() {
        assert_eq!(
            directory_from(None, None),
            PathBuf::from("cam-viewer-recordings")
        );
    }
}
