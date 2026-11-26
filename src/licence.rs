//! Copy protection: offline, signed licence keys and a 14-day trial.
//!
//! ```ignore
//! match licence::status() {
//!     Status::Licensed { name, .. } => { /* "Licensed to {name}" */ }
//!     Status::Trial { days_left } => { /* "{days_left} days left in your trial" */ }
//!     Status::TrialOver => { /* ask for a key */ }
//! }
//! let result = licence::activate(&pasted_text); // Ok(Status::Licensed{..}) or Err(message to show)
//! licence::deactivate();
//! ```
//!
//! Call these from the GUI or a background thread, never the audio thread:
//! they take a lock and may touch the disk. [`status`] is cached, so calling
//! it every frame is fine.
//!
//! # Keys
//!
//! A key (`CHOPSUEY-XXXXX-...`) carries the licensee's name and email, signed
//! with Toronto Audio Company's Ed25519 private key; the plugin embeds only
//! the public key ([`PUBLIC_KEY`]). The format lives in the `licence_format`
//! crate, shared with `tools/licence_keygen`. A key is accepted only if its
//! signature checks out, its product id is `chopsuey`, and its key id isn't
//! in [`REVOKED_KEY_IDS`]. Keys never expire and don't depend on the clock.
//! An activated key is stored in `licence.json` and re-checked on every load,
//! so editing that file just drops back to the trial.
//!
//! # Trial
//!
//! The trial runs [`TRIAL_DAYS`] days from the first time [`status`] is
//! called. Two timestamps (Unix seconds, UTC) are kept in two places:
//! `~/Library/Application Support/chopsuey/licence.json` and a mirror at
//! `~/Library/Preferences/com.torontoaudiocompany.chopsuey.plist` (a plain
//! XML plist written directly, not through NSUserDefaults; nothing else may
//! use that defaults domain). Both are written atomically (temp file and
//! rename).
//!
//! - `first_run`: the earliest value found in either file wins, and a missing
//!   or stale copy is rewritten, so deleting one file doesn't reset the trial.
//! - `last_seen`: the latest clock reading ever seen (only moves forward).
//!
//! Elapsed time is `max(now, last_seen) - first_run`, so winding the clock
//! back never buys extra days. Days left are rounded up: 14 on the first day,
//! 1 during the last 24 hours, then [`Status::TrialOver`].
//!
//! If the clock reads more than [`CLOCK_GRACE_SECS`] (one day) earlier than
//! `first_run` or `last_seen`, it has been moved back, and the trial reports
//! `TrialOver` for as long as that stays true. Nothing about this is stored:
//! once the clock is right again the remaining days come back, so an honest
//! user with a briefly wrong clock isn't locked out for good. Small backward
//! corrections (under a day, e.g. an NTP fix) are ignored. A licensed copy is
//! never affected by the clock.
//!
//! Deleting both files does restart the trial. This deters casual resets; it
//! isn't meant to stop someone willing to patch the binary.

use licence_format::KeyError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Days of free use before a key is needed.
pub const TRIAL_DAYS: u32 = 14;
/// How far the clock may read behind a stored time before it counts as
/// having been moved back.
pub const CLOCK_GRACE_SECS: i64 = DAY;
/// Product id inside every chopsuey licence key.
pub const PRODUCT_ID: &str = "chopsuey";

/// Ed25519 public key that licence keys must be signed with.
///
/// DEVELOPMENT KEYPAIR. Its private key is `tools/licence_keygen/dev-private-key.txt`
/// (git-ignored). Before release this MUST be replaced with the public key of
/// a production keypair whose private key is kept outside the repository:
/// `cargo run -p licence_keygen -- new-keypair --out <somewhere safe>` prints
/// the replacement. Keys issued with the dev keypair stop working then.
pub const PUBLIC_KEY: [u8; 32] = [
    0x47, 0x94, 0x65, 0x1f, 0xe4, 0x8d, 0x65, 0x29,
    0xc0, 0x95, 0x96, 0xd6, 0xf8, 0x9e, 0x30, 0x63,
    0x88, 0x60, 0xa6, 0xe8, 0x42, 0x04, 0x96, 0x1b,
    0xb5, 0x76, 0x51, 0x76, 0x9a, 0xb5, 0x8e, 0xf8,
];

/// Key ids of leaked or refunded keys, which are refused even though their
/// signatures are good.
pub const REVOKED_KEY_IDS: &[u32] = &[];

const DAY: i64 = 86_400;
/// How long [`status`] trusts its cached answer before looking again, so a
/// session left open overnight still counts the trial down.
const CACHE_FOR: Duration = Duration::from_secs(60 * 60);
const LICENCE_FILE: &str = "licence.json";
const MIRROR_FILE: &str = "com.torontoaudiocompany.chopsuey.plist";

/// Whether chopsuey is paid for, on trial, or neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Licensed { name: String, email: String },
    /// `days_left` is 1..=14, rounded up.
    Trial { days_left: u32 },
    /// The trial has run out, or the clock has been set back (see the module
    /// docs). A UI message might add "If your Mac's date is wrong, fix it and
    /// reopen chopsuey."
    TrialOver,
}

impl Status {
    /// Licensed or still on trial.
    pub fn is_unlocked(&self) -> bool {
        !matches!(self, Status::TrialOver)
    }
}

/// The current licence state. Cached: only the first call (and one an hour
/// after that) reads the disk.
pub fn status() -> Status {
    let mut cache = lock_cache();
    if let Some((status, at)) = cache.as_ref() {
        if at.elapsed() < CACHE_FOR {
            return status.clone();
        }
    }
    let status = Env::real().status(now());
    *cache = Some((status.clone(), Instant::now()));
    status
}

/// Checks a pasted licence key and, if it's good, saves it. The error is a
/// plain sentence to show the user as-is.
pub fn activate(key: &str) -> Result<Status, String> {
    let mut cache = lock_cache();
    let status = Env::real().activate(key, now())?;
    *cache = Some((status.clone(), Instant::now()));
    Ok(status)
}

/// Forgets the saved licence key (e.g. before moving to another computer).
/// The trial isn't restarted. Call [`status`] afterwards for the new state.
pub fn deactivate() {
    let mut cache = lock_cache();
    let env = Env::real();
    env.deactivate();
    *cache = Some((env.status(now()), Instant::now()));
}

/// Cached status and when it was worked out. The lock also keeps file
/// updates from different threads from interleaving.
static CACHE: Mutex<Option<(Status, Instant)>> = Mutex::new(None);

fn lock_cache() -> std::sync::MutexGuard<'static, Option<(Status, Instant)>> {
    CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Seconds since 1970 UTC (negative if the clock is before then).
fn now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Where things live and which key to trust. Tests point this at a scratch
/// folder and their own keypair, and pass the time in.
struct Env {
    /// Holds `licence.json`.
    dir: PathBuf,
    /// The second copy of the trial dates. `None` if there's no home folder.
    mirror: Option<PathBuf>,
    public_key: [u8; 32],
    revoked: &'static [u32],
}

/// The contents of `licence.json`. Every field is optional so older or
/// hand-edited files still load.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stored {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default)]
    first_run: Option<i64>,
    #[serde(default)]
    last_seen: Option<i64>,
}

/// The trial dates as kept in the mirror plist.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Dates {
    first_run: Option<i64>,
    last_seen: Option<i64>,
}

impl Env {
    fn real() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute());
        Self {
            // Goes through the library's helper so its one-off folder rename
            // (from the app's old name) still happens before we create ours.
            dir: crate::library::app_folder("Application Support"),
            mirror: home.map(|h| h.join("Library").join("Preferences").join(MIRROR_FILE)),
            public_key: PUBLIC_KEY,
            revoked: REVOKED_KEY_IDS,
        }
    }

    fn licence_path(&self) -> PathBuf {
        self.dir.join(LICENCE_FILE)
    }

    fn status(&self, now: i64) -> Status {
        let stored = read_stored(&self.licence_path()).unwrap_or_default();
        if let Some(key) = &stored.key {
            if let Ok(licence) = self.check_key(key) {
                return Status::Licensed { name: licence.name, email: licence.email };
            }
            // A stored key that no longer checks out (edited, revoked, or
            // signed by a retired keypair) is ignored: back to the trial.
        }
        let mirror = self.mirror.as_deref().map(read_mirror).unwrap_or_default();

        let first_run = [stored.first_run, mirror.first_run].into_iter().flatten().min().unwrap_or(now);
        let last_seen = [stored.last_seen, mirror.last_seen, Some(first_run)]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(first_run);
        let status = trial_status(first_run, last_seen, now);

        // Bring both copies up to date. Failures are fine: the dates are
        // simply worked out again next time.
        let dates = Dates { first_run: Some(first_run), last_seen: Some(last_seen.max(now)) };
        if (stored.first_run, stored.last_seen) != (dates.first_run, dates.last_seen) {
            let updated = Stored { first_run: dates.first_run, last_seen: dates.last_seen, ..stored };
            let _ = write_stored(&self.licence_path(), &updated);
        }
        if let Some(path) = &self.mirror {
            if mirror != dates {
                let _ = write_atomic(path, mirror_plist(dates).as_bytes());
            }
        }
        status
    }

    fn activate(&self, key: &str, now: i64) -> Result<Status, String> {
        let licence = self.check_key(key).map_err(|e| e.message())?;

        let path = self.licence_path();
        let mut stored = read_stored(&path).unwrap_or_default();
        stored.key = Some(key.trim().to_string());
        // Record the trial start too if it never ran, so deactivating later
        // doesn't hand out a fresh trial.
        if stored.first_run.is_none() {
            let mirror = self.mirror.as_deref().map(read_mirror).unwrap_or_default();
            stored.first_run = Some(mirror.first_run.map_or(now, |m| m.min(now)));
        }
        write_stored(&path, &stored).map_err(|e| {
            format!(
                "Your licence key is valid, but chopsuey couldn't save it ({e}). \
                 Check that the folder {} can be written to, then try again.",
                self.dir.display()
            )
        })?;
        Ok(Status::Licensed { name: licence.name, email: licence.email })
    }

    fn deactivate(&self) {
        let path = self.licence_path();
        // An unreadable file has no usable key in it, so leave it alone.
        let Ok(mut stored) = read_stored(&path) else { return };
        if stored.key.take().is_some() {
            let _ = write_stored(&path, &stored);
        }
    }

    fn check_key(&self, key: &str) -> Result<licence_format::Licence, ActivateError> {
        if key.trim().is_empty() {
            return Err(ActivateError::Empty);
        }
        let licence = licence_format::verify(key, PRODUCT_ID, &self.public_key).map_err(ActivateError::Key)?;
        if self.revoked.contains(&licence.key_id) {
            return Err(ActivateError::Revoked);
        }
        Ok(licence)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ActivateError {
    Empty,
    Key(KeyError),
    Revoked,
}

impl ActivateError {
    /// Plain words for the activation box.
    fn message(&self) -> String {
        match self {
            ActivateError::Empty => "Paste your licence key into the box first.".into(),
            ActivateError::Key(KeyError::NotAKey) => {
                "That doesn't look like a chopsuey licence key. Copy the whole key from your \
                 purchase email (it starts with CHOPSUEY-) and paste it again."
                    .into()
            }
            ActivateError::Key(KeyError::BadSignature) => {
                "That licence key isn't valid. Check that you copied all of it, with nothing \
                 missing or added, and try again."
                    .into()
            }
            ActivateError::Key(KeyError::WrongProduct(product)) => format!(
                "That licence key is for a different product ({product}), not chopsuey."
            ),
            ActivateError::Key(KeyError::UnsupportedVersion(_)) => {
                "That licence key is for a newer version of chopsuey. Please update chopsuey \
                 and try again."
                    .into()
            }
            ActivateError::Key(KeyError::Malformed) => {
                "That licence key couldn't be read. Please contact Toronto Audio Company for \
                 a replacement."
                    .into()
            }
            ActivateError::Revoked => {
                "That licence key has been cancelled. Please contact Toronto Audio Company."
                    .into()
            }
        }
    }
}

/// The trial rules from the module docs.
fn trial_status(first_run: i64, last_seen: i64, now: i64) -> Status {
    if now < first_run - CLOCK_GRACE_SECS || now < last_seen - CLOCK_GRACE_SECS {
        return Status::TrialOver;
    }
    let elapsed = (now.max(last_seen) - first_run).max(0);
    let remaining = TRIAL_DAYS as i64 * DAY - elapsed;
    if remaining <= 0 {
        return Status::TrialOver;
    }
    Status::Trial { days_left: ((remaining + DAY - 1) / DAY) as u32 }
}

/// `Ok(default)` if the file doesn't exist; `Err` if it exists but can't be
/// read or parsed.
fn read_stored(path: &Path) -> Result<Stored, ()> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| ()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Stored::default()),
        Err(_) => Err(()),
    }
}

fn write_stored(path: &Path, stored: &Stored) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(stored).map_err(std::io::Error::other)?;
    write_atomic(path, &json)
}

/// Temp file + rename, so a crash can't leave half a file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn mirror_plist(dates: Dates) -> String {
    let mut entries = String::new();
    for (key, value) in [("FirstRun", dates.first_run), ("LastSeen", dates.last_seen)] {
        if let Some(v) = value {
            entries.push_str(&format!("\t<key>{key}</key>\n\t<integer>{v}</integer>\n"));
        }
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n{entries}</dict>\n</plist>\n"
    )
}

/// Reads what [`mirror_plist`] wrote. Anything missing or unreadable is `None`.
fn read_mirror(path: &Path) -> Dates {
    let Ok(text) = std::fs::read_to_string(path) else { return Dates::default() };
    let integer_after = |key: &str| -> Option<i64> {
        let after_key = text.split_once(&format!("<key>{key}</key>"))?.1;
        let value = after_key.trim_start().strip_prefix("<integer>")?;
        value.split_once("</integer>")?.0.trim().parse().ok()
    };
    Dates { first_run: integer_after("FirstRun"), last_seen: integer_after("LastSeen") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A throwaway keypair, so tests can mint keys freely.
    const TEST_SEED: [u8; 32] = [42; 32];
    /// Issued with `tools/licence_keygen/dev-private-key.txt`:
    /// `keygen issue --key dev-private-key.txt --name "Dev Tester"
    /// --email dev@torontoaudio.example --id 1 --date 2026-11-25`
    const DEV_KEY: &str = "CHOPSUEY-04466-T3FE1-SQASB-S0000-00800-18JW2-J4CNV-20N35-EDT6A-WGRCH-JQCG3-MDXS6-YVKMD-XGQAS-39DWQ-6AY31-DNR6R-SC100-607S4-2S9S7-EZS5G-NHDDR-KWY50-GX4HM-SQRNT-74E7F-S8W4J-MQ8R6-3FPYN-XAWFX-DJKJZ-NV3FE-ZMG52-8A0WZ-9QSAF-WDRRJ-CQT66-RGG2";

    const T0: i64 = 1_790_000_000; // some time in 2026
    static REVOKED_FOR_TESTS: &[u32] = &[666];

    struct Scratch {
        root: PathBuf,
        env: Env,
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn scratch() -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "chopsuey-licence-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let env = Env {
            dir: root.join("Application Support").join("chopsuey"),
            mirror: Some(root.join("Preferences").join(MIRROR_FILE)),
            public_key: licence_format::public_key_from_seed(&TEST_SEED),
            revoked: REVOKED_FOR_TESTS,
        };
        Scratch { root, env }
    }

    fn key_for(product: &str, name: &str, email: &str, key_id: u32) -> String {
        let l = licence_format::Licence::new(product, name, email, key_id, 20_782);
        licence_format::issue(&l, &TEST_SEED).unwrap()
    }

    fn good_key() -> String {
        key_for("chopsuey", "Ada Lovelace", "ada@example.com", 7)
    }

    fn licensed(name: &str, email: &str) -> Status {
        Status::Licensed { name: name.into(), email: email.into() }
    }

    fn days(n: i64) -> i64 {
        n * DAY
    }

    // ---- keys -------------------------------------------------------------

    #[test]
    fn dev_keypair_key_activates_with_embedded_public_key() {
        let s = scratch();
        let env = Env { public_key: PUBLIC_KEY, revoked: REVOKED_KEY_IDS, ..s.env.clone_paths() };
        let expected = licensed("Dev Tester", "dev@torontoaudio.example");
        assert_eq!(env.activate(DEV_KEY, T0), Ok(expected.clone()));
        assert_eq!(env.status(T0), expected);
    }

    #[test]
    fn valid_key_activates_and_survives_a_reload() {
        let s = scratch();
        assert_eq!(s.env.status(T0), Status::Trial { days_left: 14 });
        let status = s.env.activate(&good_key(), T0 + 10).unwrap();
        assert_eq!(status, licensed("Ada Lovelace", "ada@example.com"));
        // A fresh look at the disk (as on the next plugin load) agrees, long
        // after the trial would have ended.
        assert_eq!(s.env.status(T0 + days(400)), status);
        let stored = read_stored(&s.env.licence_path()).unwrap();
        assert_eq!(stored.key.as_deref(), Some(good_key().as_str()));
        assert_eq!(stored.first_run, Some(T0), "activation keeps the trial dates");
    }

    #[test]
    fn pasted_with_spaces_lowercase_and_line_breaks() {
        let key = good_key();
        let body = key.strip_prefix("CHOPSUEY-").unwrap().to_string();
        for pasted in [
            key.to_lowercase(),
            format!("\n  {key}  \n"),
            key.replace('-', " "),
            key.replace('-', ""),
            body.clone(),
            body.replace('-', "").to_lowercase(),
            format!("{}\n{}", &key[..40], &key[40..]),
        ] {
            let s = scratch();
            assert_eq!(
                s.env.activate(&pasted, T0),
                Ok(licensed("Ada Lovelace", "ada@example.com")),
                "{pasted:?}"
            );
            assert!(s.env.status(T0).is_unlocked());
        }
    }

    #[test]
    fn tampered_key_is_refused_and_nothing_is_saved() {
        let s = scratch();
        let key = good_key();
        // Change one character in the middle of the data.
        let i = (key.len() / 2..).find(|&i| &key[i..i + 1] != "-").unwrap();
        let c = if &key[i..i + 1] == "Z" { "Y" } else { "Z" };
        let tampered = format!("{}{}{}", &key[..i], c, &key[i + 1..]);
        let err = s.env.activate(&tampered, T0).unwrap_err();
        assert!(err.contains("isn't valid"), "{err}");
        assert_eq!(read_stored(&s.env.licence_path()).unwrap().key, None);

        // Cut short, as if only part of it was copied.
        assert!(s.env.activate(&key[..key.len() - 10], T0).is_err());
        // Signed by someone else's private key.
        let forged = licence_format::issue(
            &licence_format::Licence::new("chopsuey", "Mallory", "m@example.com", 1, 1),
            &[1; 32],
        )
        .unwrap();
        assert!(s.env.activate(&forged, T0).unwrap_err().contains("isn't valid"));
        assert_eq!(s.env.status(T0), Status::Trial { days_left: 14 });
    }

    #[test]
    fn stored_key_edited_on_disk_falls_back_to_trial() {
        let s = scratch();
        s.env.activate(&good_key(), T0).unwrap();
        let mut stored = read_stored(&s.env.licence_path()).unwrap();
        let key = stored.key.take().unwrap();
        stored.key = Some(key.replacen("CHOPSUEY-0", "CHOPSUEY-1", 1));
        write_stored(&s.env.licence_path(), &stored).unwrap();
        assert_eq!(s.env.status(T0 + days(1)), Status::Trial { days_left: 13 });
    }

    #[test]
    fn wrong_product_is_explained() {
        let s = scratch();
        let err = s.env.activate(&key_for("otherplug", "Ada", "a@example.com", 1), T0).unwrap_err();
        assert!(err.contains("different product (otherplug)"), "{err}");
    }

    #[test]
    fn junk_empty_and_revoked_keys_are_explained() {
        let s = scratch();
        assert!(s.env.activate("", T0).unwrap_err().contains("Paste your licence key"));
        assert!(s.env.activate("   \n", T0).unwrap_err().contains("Paste your licence key"));
        assert!(s.env.activate("hello there", T0).unwrap_err().contains("doesn't look like"));
        let revoked = key_for("chopsuey", "Leaky", "leak@example.com", 666);
        assert!(s.env.activate(&revoked, T0).unwrap_err().contains("cancelled"));
    }

    #[test]
    fn deactivate_returns_to_the_original_trial() {
        let s = scratch();
        s.env.status(T0);
        s.env.activate(&good_key(), T0 + days(3)).unwrap();
        s.env.deactivate();
        assert_eq!(s.env.status(T0 + days(3)), Status::Trial { days_left: 11 });
        assert_eq!(s.env.status(T0 + days(20)), Status::TrialOver);
        // Deactivating twice, or with nothing saved, is harmless.
        s.env.deactivate();
        scratch().env.deactivate();
    }

    #[test]
    fn activating_before_any_trial_still_records_one() {
        let s = scratch();
        s.env.activate(&good_key(), T0).unwrap();
        s.env.deactivate();
        assert_eq!(s.env.status(T0 + days(15)), Status::TrialOver);
    }

    #[test]
    fn licence_ignores_the_clock() {
        let s = scratch();
        s.env.status(T0);
        s.env.activate(&good_key(), T0).unwrap();
        assert!(matches!(s.env.status(T0 - days(365)), Status::Licensed { .. }));
    }

    // ---- trial ------------------------------------------------------------

    #[test]
    fn trial_counts_down_and_ends() {
        let s = scratch();
        let e = &s.env;
        assert_eq!(e.status(T0), Status::Trial { days_left: 14 });
        assert_eq!(e.status(T0 + 1), Status::Trial { days_left: 14 });
        assert_eq!(e.status(T0 + days(1) - 1), Status::Trial { days_left: 14 });
        assert_eq!(e.status(T0 + days(1)), Status::Trial { days_left: 13 });
        assert_eq!(e.status(T0 + days(13)), Status::Trial { days_left: 1 });
        assert_eq!(e.status(T0 + days(14) - 1), Status::Trial { days_left: 1 });
        assert_eq!(e.status(T0 + days(14)), Status::TrialOver);
        assert_eq!(e.status(T0 + days(100)), Status::TrialOver);
    }

    #[test]
    fn first_run_is_written_to_both_places() {
        let s = scratch();
        s.env.status(T0);
        let stored = read_stored(&s.env.licence_path()).unwrap();
        assert_eq!((stored.first_run, stored.last_seen), (Some(T0), Some(T0)));
        let mirror = read_mirror(s.env.mirror.as_ref().unwrap());
        assert_eq!(mirror, Dates { first_run: Some(T0), last_seen: Some(T0) });
        let text = std::fs::read_to_string(s.env.mirror.as_ref().unwrap()).unwrap();
        assert!(text.starts_with("<?xml") && text.contains("<plist version=\"1.0\">"));
    }

    #[test]
    fn deleting_licence_json_keeps_the_trial() {
        let s = scratch();
        s.env.status(T0);
        std::fs::remove_file(s.env.licence_path()).unwrap();
        assert_eq!(s.env.status(T0 + days(5)), Status::Trial { days_left: 9 });
        // ...and the deleted copy is put back.
        assert_eq!(read_stored(&s.env.licence_path()).unwrap().first_run, Some(T0));
    }

    #[test]
    fn deleting_the_mirror_keeps_the_trial() {
        let s = scratch();
        s.env.status(T0);
        let mirror = s.env.mirror.clone().unwrap();
        std::fs::remove_file(&mirror).unwrap();
        assert_eq!(s.env.status(T0 + days(5)), Status::Trial { days_left: 9 });
        assert_eq!(read_mirror(&mirror).first_run, Some(T0));
    }

    #[test]
    fn deleting_the_whole_app_support_folder_keeps_the_trial() {
        let s = scratch();
        s.env.status(T0);
        std::fs::remove_dir_all(&s.env.dir).unwrap();
        assert_eq!(s.env.status(T0 + days(15)), Status::TrialOver);
    }

    #[test]
    fn earliest_first_run_wins() {
        let s = scratch();
        s.env.status(T0);
        // Someone edits licence.json to restart the clock; the mirror remembers.
        let stored = Stored { first_run: Some(T0 + days(20)), last_seen: Some(T0 + days(20)), key: None };
        write_stored(&s.env.licence_path(), &stored).unwrap();
        assert_eq!(s.env.status(T0 + days(20)), Status::TrialOver);
        assert_eq!(read_stored(&s.env.licence_path()).unwrap().first_run, Some(T0));
    }

    #[test]
    fn corrupt_files_do_not_panic() {
        let s = scratch();
        s.env.status(T0);
        std::fs::write(s.env.licence_path(), b"{ not json").unwrap();
        std::fs::write(s.env.mirror.as_ref().unwrap(), b"\xff\xfe garbage").unwrap();
        // Both copies unreadable: a fresh trial is the only option.
        assert_eq!(s.env.status(T0 + days(2)), Status::Trial { days_left: 14 });
        // And they were rewritten in good shape.
        assert_eq!(read_stored(&s.env.licence_path()).unwrap().first_run, Some(T0 + days(2)));
        s.env.deactivate();
        assert!(s.env.activate(&good_key(), T0).is_ok());
    }

    #[test]
    fn one_corrupt_copy_is_repaired_from_the_other() {
        let s = scratch();
        s.env.status(T0);
        std::fs::write(s.env.licence_path(), b"garbage").unwrap();
        assert_eq!(s.env.status(T0 + days(2)), Status::Trial { days_left: 12 });
        assert_eq!(read_stored(&s.env.licence_path()).unwrap().first_run, Some(T0));
    }

    #[test]
    fn unwritable_folder_is_handled() {
        let s = scratch();
        // A plain file where the folders should be: nothing can be created.
        std::fs::create_dir_all(&s.root).unwrap();
        std::fs::write(s.root.join("blocker"), b"").unwrap();
        let env = Env {
            dir: s.root.join("blocker").join("chopsuey"),
            mirror: Some(s.root.join("blocker").join(MIRROR_FILE)),
            public_key: licence_format::public_key_from_seed(&TEST_SEED),
            revoked: &[],
        };
        assert_eq!(env.status(T0), Status::Trial { days_left: 14 });
        let err = env.activate(&good_key(), T0).unwrap_err();
        assert!(err.contains("couldn't save it"), "{err}");
        env.deactivate();
        // No home folder at all: no mirror, still fine.
        let env = Env { mirror: None, ..env };
        assert_eq!(env.status(T0), Status::Trial { days_left: 14 });
    }

    #[test]
    fn clock_moved_back_ends_the_trial_until_it_is_fixed() {
        let s = scratch();
        let e = &s.env;
        assert_eq!(e.status(T0), Status::Trial { days_left: 14 });
        assert_eq!(e.status(T0 + days(10)), Status::Trial { days_left: 4 });
        // Wound back to just after install, to try to get the days back.
        assert_eq!(e.status(T0 + days(1)), Status::TrialOver);
        // Wound back to before install.
        assert_eq!(e.status(T0 - days(30)), Status::TrialOver);
        // Nothing was stored about it: with the clock right, the days are back.
        assert_eq!(e.status(T0 + days(10) + 60), Status::Trial { days_left: 4 });
        let stored = read_stored(&e.licence_path()).unwrap();
        assert_eq!(stored.last_seen, Some(T0 + days(10) + 60));
    }

    #[test]
    fn small_clock_corrections_are_forgiven_but_buy_nothing() {
        let s = scratch();
        let e = &s.env;
        e.status(T0);
        e.status(T0 + days(13) + 3600); // 1 day left, 23h to go
        // Clock nudged back by 20 hours: not treated as tampering, and the
        // trial still counts from the latest time seen.
        assert_eq!(e.status(T0 + days(12) + 7200), Status::Trial { days_left: 1 });
        assert_eq!(e.status(T0 + days(14)), Status::TrialOver);
    }

    #[test]
    fn mirror_plist_round_trips() {
        let s = scratch();
        let path = s.root.join("x.plist");
        for dates in [
            Dates { first_run: Some(T0), last_seen: Some(T0 + 5) },
            Dates { first_run: Some(-3), last_seen: None },
            Dates::default(),
        ] {
            write_atomic(&path, mirror_plist(dates).as_bytes()).unwrap();
            assert_eq!(read_mirror(&path), dates);
        }
        assert_eq!(read_mirror(&s.root.join("missing.plist")), Dates::default());
    }

    #[test]
    fn status_helpers() {
        assert!(Status::Trial { days_left: 3 }.is_unlocked());
        assert!(licensed("a", "b").is_unlocked());
        assert!(!Status::TrialOver.is_unlocked());
    }

    impl Env {
        fn clone_paths(&self) -> Env {
            Env {
                dir: self.dir.clone(),
                mirror: self.mirror.clone(),
                public_key: self.public_key,
                revoked: self.revoked,
            }
        }
    }
}
