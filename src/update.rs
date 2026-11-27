//! Telling the user a newer chopsuey is out (a line at the bottom of the
//! sheet that opens the release page).
//!
//! At most once a day, asks GitHub for the latest release of the public
//! repository and compares its tag with this build's version. The request
//! carries nothing about the user: it's the same anonymous GET anyone
//! visiting the releases page makes. It uses the system's `curl`, so the
//! plugin needs no TLS library, and runs on a background thread.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::safety::LockExt;

/// The repository releases are published to. It must be public: the check
/// is anonymous, so a private repository answers "not found".
pub const LATEST_RELEASE_API: &str = "https://api.github.com/repos/Toronto-Audio-Company/chopsuey/releases/latest";

const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Release {
    /// "1.2.0" (a leading "v" in the tag is dropped).
    pub version: String,
    /// The release page, where the installer is.
    pub url: String,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Cache {
    checked_at: u64,
    latest: Option<Release>,
}

static AVAILABLE: Mutex<Option<Release>> = Mutex::new(None);

/// A newer release than this build, once a check has found one.
pub fn available() -> Option<Release> {
    AVAILABLE.locked().clone()
}

fn cache_path() -> PathBuf {
    crate::library::app_folder("Caches").join("update.json")
}

/// Checks in the background, unless the last check (by any instance) was
/// less than a day ago, in which case its answer is reused.
pub fn check_in_background() {
    crate::safety::spawn("chopsuey-update-check", || {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let cached: Cache = std::fs::read(cache_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let latest = if now.saturating_sub(cached.checked_at) < CHECK_EVERY.as_secs() {
            cached.latest
        } else {
            let latest = match fetch_latest() {
                Ok(r) => Some(r),
                Err(e) => {
                    crate::logln!("update check failed: {e}");
                    return; // try again next time
                }
            };
            let cache = Cache { checked_at: now, latest: latest.clone() };
            if let Ok(json) = serde_json::to_vec(&cache) {
                let path = cache_path();
                let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if std::fs::write(&tmp, json).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
            latest
        };
        *AVAILABLE.locked() = latest.filter(|r| is_newer(&r.version, env!("CARGO_PKG_VERSION")));
    });
}

fn fetch_latest() -> Result<Release, String> {
    let out = std::process::Command::new("/usr/bin/curl")
        .args(["--fail", "--silent", "--show-error", "--location", "--max-time", "15"])
        .args(["--header", "Accept: application/vnd.github+json"])
        .args(["--user-agent", concat!("chopsuey/", env!("CARGO_PKG_VERSION"))])
        .arg(LATEST_RELEASE_API)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    parse_release(&out.stdout)
}

fn parse_release(json: &[u8]) -> Result<Release, String> {
    let v: serde_json::Value = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    let tag = v["tag_name"].as_str().ok_or("no tag_name")?;
    let url = v["html_url"].as_str().ok_or("no html_url")?;
    Ok(Release { version: tag.trim_start_matches('v').to_string(), url: url.to_string() })
}

/// Whether `candidate` is a later version than `current` ("1.10.0" >
/// "1.9.2"). Pre-release tags ("1.2.0-beta.1") are never offered.
fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |s: &str| -> Option<Vec<u64>> { s.split('.').map(|p| p.parse().ok()).collect() };
    match (parse(candidate), parse(current.split('-').next().unwrap_or(current))) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.10.0", "1.9.2"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("1.2.0-beta.1", "0.1.0"), "pre-releases aren't offered");
        assert!(!is_newer("nonsense", "0.1.0"));
    }

    #[test]
    fn reads_github_release() {
        let json = br#"{"tag_name":"v1.2.0","html_url":"https://github.com/x/y/releases/tag/v1.2.0","assets":[]}"#;
        assert_eq!(
            parse_release(json).unwrap(),
            Release { version: "1.2.0".into(), url: "https://github.com/x/y/releases/tag/v1.2.0".into() }
        );
        assert!(parse_release(b"{}").is_err());
    }
}
