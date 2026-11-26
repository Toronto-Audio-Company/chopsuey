//! Stems: the four parts (drums, bass, other, vocals) a song is split into,
//! their disk cache, and running the split.
//!
//! The split itself (HTDemucs on ONNX Runtime) runs in a separate process,
//! `chopsuey-stems` (the `stems_helper` crate), next to the plugin binary in
//! the bundle. If ONNX Runtime or the GPU driver crashes, only the helper
//! dies and the host keeps running; and the model's memory is freed when a
//! split ends instead of staying in the host for the session.
//!
//! The model weights' licence is unclear (possibly research-only), so this
//! is for personal use; swap the model before distributing the plugin.

use crate::safety::LockExt;
use std::path::Path;


/// Stem order as the model outputs them.
pub const STEM_NAMES: [&str; 4] = ["drums", "bass", "other", "vocals"];
pub const MODEL_SAMPLE_RATE: u32 = 44_100;
/// The model's fixed window: 7.8 s at 44.1 kHz.
pub const SEGMENT: usize = 343_980;

/// Four stems, each interleaved stereo at the source sample rate and the
/// same length as the source, in `STEM_NAMES` order.
pub struct Stems {
    pub sample_rate: u32,
    pub stems: [Vec<f32>; 4],
}

// ---------------------------------------------------------------------------
// Model location and the shared separator
// ---------------------------------------------------------------------------

/// File name of the model inside the plugin bundle's `Contents/Resources`.
pub const MODEL_FILE: &str = "htdemucs.onnx";

/// Where the installer puts the stem splitter (helper, Dawn and model) once
/// for every format, instead of a copy inside each plugin bundle.
pub const SUPPORT_DIR: &str = "/Library/Application Support/Toronto Audio Company/chopsuey";

/// The model: `TEMPO_SAMPLER_MODEL` if set (tests, development), else
/// `Contents/Resources/htdemucs.onnx` inside the plugin bundle (development
/// bundles), else the installer's support folder.
pub fn model_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("TEMPO_SAMPLER_MODEL") {
        return Some(p.into());
    }
    let in_bundle = own_binary_path() // .../chopsuey.vst3/Contents/MacOS/chopsuey
        .and_then(|b| Some(b.parent()?.parent()?.join("Resources").join(MODEL_FILE)));
    let installed = Path::new(SUPPORT_DIR).join(MODEL_FILE);
    in_bundle.into_iter().chain([installed]).find(|p| p.is_file())
}

/// Path of the dylib this code lives in (not the host's executable).
#[cfg(unix)]
fn own_binary_path() -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    let addr = own_binary_path as *const libc::c_void;
    // SAFETY: dladdr only reads the address; info is a plain C struct.
    if unsafe { libc::dladdr(addr, &mut info) } == 0 || info.dli_fname.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) };
    Some(std::ffi::OsStr::from_bytes(name.to_bytes()).into())
}

#[cfg(not(unix))]
fn own_binary_path() -> Option<std::path::PathBuf> {
    None
}

/// File name of the helper executable, next to the plugin binary.
pub const HELPER_FILE: &str = "chopsuey-stems";

/// The helper: `CHOPSUEY_STEMS_HELPER` if set, else next to this binary (a
/// development bundle's `Contents/MacOS`), else the installer's support
/// folder, else the cargo target folder above this binary (tests run from
/// `target/<profile>/deps`).
pub fn helper_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("CHOPSUEY_STEMS_HELPER") {
        return Some(p.into());
    }
    let binary = own_binary_path()?;
    let beside = binary.parent().map(|dir| dir.join(HELPER_FILE));
    let installed = Path::new(SUPPORT_DIR).join(HELPER_FILE);
    let target = binary.ancestors().nth(2).map(|dir| dir.join(HELPER_FILE));
    beside.into_iter().chain([installed]).chain(target).find(|p| p.is_file())
}

/// Splits `input` into stems saved in `out` (see `load_cached`), by running
/// the helper. `progress` gets 0..1. `Ok(false)` if `cancelled()` became
/// true, which stops the helper at once.
///
/// One split runs at a time per host process, however many plugin
/// instances ask (each needs ~1 GB and the GPU); the others wait their turn.
pub fn split(input: &Path, out: &Path, mut progress: impl FnMut(f32), cancelled: impl Fn() -> bool) -> Result<bool, String> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _turn = loop {
        match ONE_AT_A_TIME.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) if cancelled() => return Ok(false),
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(100)),
        }
    };

    // ONNX Runtime has no Intel Mac build, so the helper is arm64 only. (An
    // Intel build of the plugin under Rosetta still starts it natively.)
    if !apple_silicon() {
        return Err("Splitting into stems needs a Mac with Apple silicon.".into());
    }
    let helper = helper_path().ok_or("The stem splitter is missing from the plugin. Reinstall chopsuey.")?;
    let model = model_path().ok_or("The stem model is missing from the plugin. Reinstall chopsuey.")?;
    let mut child = Command::new(&helper)
        .arg("--model")
        .arg(&model)
        .arg("--input")
        .arg(input)
        .arg("--out")
        .arg(out)
        // Kept open: the helper quits when it closes (us going away).
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't start the stem splitter ({e})."))?;

    // Progress as f32 bits; the last "error" line, for the message.
    let shared_progress = std::sync::Arc::new(AtomicU32::new(0));
    let error_line = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let reader = child.stdout.take().map(|stdout| {
        let (progress, error_line) = (shared_progress.clone(), error_line.clone());
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(p) = line.strip_prefix("progress ").and_then(|p| p.parse::<f32>().ok()) {
                    progress.store(p.to_bits(), Ordering::Relaxed);
                } else if let Some(e) = line.strip_prefix("error ") {
                    *error_line.locked() = Some(e.to_string());
                } else if let Some(d) = line.strip_prefix("devices ") {
                    crate::logln!("stems: splitting on {d}");
                }
            }
        })
    });
    // ONNX Runtime's own messages go to the log.
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                crate::logln!("stems helper: {line}");
            }
        });
    }

    let status = loop {
        progress(f32::from_bits(shared_progress.load(Ordering::Relaxed)));
        if cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(false);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("Lost track of the stem splitter ({e}).")),
        }
    };
    if let Some(r) = reader {
        let _ = r.join();
    }
    if status.success() {
        return Ok(true);
    }
    use std::os::unix::process::ExitStatusExt;
    let message = match (error_line.locked().take(), status.signal()) {
        (Some(e), _) => e,
        (None, Some(sig)) => {
            crate::logln!("stems helper killed by signal {sig}");
            "The stem splitter crashed. Your set is fine; try again, and if it keeps happening send the log (Report a problem).".to_string()
        }
        (None, None) => format!("The stem splitter stopped ({status})."),
    };
    Err(message)
}

/// The hardware is Apple silicon, whatever architecture this code runs as.
fn apple_silicon() -> bool {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: a fixed C string name and an int-sized out buffer.
    let found = unsafe {
        libc::sysctlbyname(c"hw.optional.arm64".as_ptr(), (&mut value as *mut libc::c_int).cast(), &mut size, std::ptr::null_mut(), 0)
    };
    found == 0 && value == 1
}

// ---------------------------------------------------------------------------
// Disk cache
// ---------------------------------------------------------------------------

/// Stems are kept here as 16-bit WAVs, one folder per source file.
pub fn cache_root() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("TEMPO_SAMPLER_STEM_CACHE") {
        return p.into();
    }
    crate::library::app_folder("Caches").join("stems")
}

/// Oldest-used stems are deleted once the cache grows past this.
pub const CACHE_LIMIT_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Cache folder for a source file, keyed by path, size and modification
/// time (so an edited file is split again).
pub fn cache_dir_for(source: &Path) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let meta = std::fs::metadata(source).ok()?;
    let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    // FNV-1a: unlike std's DefaultHasher, the same on every Rust version,
    // so a toolchain update doesn't orphan everyone's cached stems.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let parts: [&[u8]; 4] = [source.as_os_str().as_bytes(), &meta.len().to_le_bytes(), &modified.to_le_bytes(), MODEL_FILE.as_bytes()];
    for part in parts {
        for &b in part.iter().chain(&[0xff]) {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    Some(cache_root().join(format!("{h:016x}")))
}

/// Loads cached stems, or `None` if they aren't all there. The four files
/// are read and decoded in parallel.
pub fn load_cached(dir: &Path) -> Option<Stems> {
    let loaded: Vec<Option<(Vec<f32>, u32)>> = std::thread::scope(|scope| {
        let jobs: Vec<_> = STEM_NAMES
            .iter()
            .map(|name| {
                let path = dir.join(format!("{name}.wav"));
                scope.spawn(move || {
                    let bytes = std::fs::read(path).ok()?;
                    crate::loops::decode_wav_bytes(&bytes).ok()
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().ok().flatten()).collect()
    });
    let mut stems = Vec::with_capacity(4);
    let mut sample_rate = 0;
    for l in loaded {
        let (s, sr) = l?;
        sample_rate = sr;
        stems.push(s);
    }
    // Mark as recently used, for eviction.
    let _ = std::fs::write(dir.join("used"), b"");
    let stems: [Vec<f32>; 4] = stems.try_into().ok()?;
    Some(Stems { sample_rate, stems })
}

/// Saves stems atomically (temp folder + rename), then trims the cache.
pub fn save_cached(dir: &Path, stems: &Stems) -> Result<(), String> {
    let tmp = dir.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: stems.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    // Four files in parallel, each packed in one buffer and written in one
    // go (same 16-bit values as writing sample by sample).
    std::thread::scope(|scope| {
        let jobs: Vec<_> = STEM_NAMES
            .iter()
            .zip(&stems.stems)
            .map(|(name, s)| {
                let path = tmp.join(format!("{name}.wav"));
                scope.spawn(move || {
                    let mut data = Vec::with_capacity(s.len() * 2);
                    for v in s {
                        data.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
                    }
                    crate::loops::write_wav_pcm(&path, spec, &data, &[]).map_err(|e| e.to_string())
                })
            })
            .collect();
        jobs.into_iter().try_for_each(|j| j.join().map_err(|_| "stem writer panicked".to_string())?)
    })?;
    std::fs::write(tmp.join("used"), b"").map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&tmp, dir).map_err(|e| e.to_string())?;
    evict(dir);
    Ok(())
}

/// Deletes least-recently-used stem folders (never `keep`) until the cache
/// is under `CACHE_LIMIT_BYTES`.
fn evict(keep: &Path) {
    let Ok(read) = std::fs::read_dir(cache_root()) else { return };
    let mut dirs: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> = read
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let path = e.path();
            let used = std::fs::metadata(path.join("used")).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            let size = std::fs::read_dir(&path)
                .map(|r| r.flatten().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum())
                .unwrap_or(0);
            (used, size, path)
        })
        .collect();
    let mut total: u64 = dirs.iter().map(|d| d.1).sum();
    dirs.sort_by_key(|d| d.0);
    for (_, size, path) in dirs {
        if total <= CACHE_LIMIT_BYTES {
            break;
        }
        if path != keep && std::fs::remove_dir_all(&path).is_ok() {
            total -= size;
        }
    }
}

/// Sums the stems selected by `mask` (bit i = `STEM_NAMES[i]`) over frames
/// `start..end`, as interleaved stereo.
pub fn mix(stems: &Stems, mask: u8, start: usize, end: usize) -> Vec<f32> {
    let frames = stems.stems[0].len() / 2;
    let start = start.min(frames);
    let end = end.clamp(start, frames);
    let mut out = vec![0.0f32; (end - start) * 2];
    for (i, s) in stems.stems.iter().enumerate() {
        if mask & (1 << i) != 0 {
            for (o, v) in out.iter_mut().zip(&s[start * 2..end * 2]) {
                *o += v;
            }
        }
    }
    out
}


#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, hz: f32) -> Vec<f32> {
        (0..frames * 2).map(|i| (i as f32 / 2.0 * hz / 48_000.0 * std::f32::consts::TAU).sin() * 0.5).collect()
    }

    #[test]
    fn cache_round_trip_and_mix() {
        let st = Stems { sample_rate: 48_000, stems: [tone(9_000, 100.0), tone(9_000, 200.0), tone(9_000, 300.0), tone(9_000, 400.0)] };
        let dir = std::env::temp_dir().join(format!("tempo-sampler-stems-{}", std::process::id()));
        save_cached(&dir, &st).unwrap();
        let back = load_cached(&dir).unwrap();
        let err = back.stems[0].iter().zip(&st.stems[0]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(err < 1e-3, "16-bit cache error {err}");
        let _ = std::fs::remove_dir_all(&dir);

        let both = mix(&st, 0b0011, 0, 9_000);
        let sum = st.stems[0][10] + st.stems[1][10];
        assert!((both[10] - sum).abs() < 1e-6);
    }

    #[test]
    fn knows_the_hardware() {
        assert_eq!(apple_silicon(), cfg!(target_arch = "aarch64") || std::process::Command::new("sysctl").args(["-n", "hw.optional.arm64"]).output().is_ok_and(|o| o.stdout.starts_with(b"1")));
    }

    #[test]
    fn cache_folder_is_stable() {
        let file = std::env::temp_dir().join(format!("tempo-sampler-hash-{}", std::process::id()));
        std::fs::write(&file, b"abc").unwrap();
        // Folder names only: another test points the cache root elsewhere.
        let name = |f: &Path| cache_dir_for(f).unwrap().file_name().unwrap().to_owned();
        let a = name(&file);
        assert_eq!(a, name(&file));
        std::fs::write(&file, b"abcd").unwrap();
        assert_ne!(a, name(&file), "an edited file is split again");
        let _ = std::fs::remove_file(&file);
    }
}
