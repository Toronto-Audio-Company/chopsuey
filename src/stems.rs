//! Stem separation with HTDemucs (4 stems) running on ONNX Runtime.
//!
//! The model weights' licence is unclear (possibly research-only), so this
//! is for personal use; swap the model before distributing the plugin.

use std::path::Path;

use ort::session::Session;
use ort::value::Tensor;

use crate::dsp::resample_stereo;

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

/// Which hardware runs the model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Engine {
    /// Run windows on the GPU through ONNX Runtime's WebGPU backend (Metal
    /// on macOS, fp32). Falls back to CPU if WebGPU can't start.
    pub gpu: bool,
    /// Also run windows on the CPU with this many threads, in parallel with
    /// the GPU (0 = GPU only). The two take windows from one queue.
    pub cpu_threads: usize,
}

impl Engine {
    pub const CPU_ONLY: Engine = Engine { gpu: false, cpu_threads: 6 };
}

/// One model instance and the device it runs on.
struct Worker {
    label: &'static str,
    session: std::sync::Mutex<Session>,
}

/// Runs HTDemucs on one or more devices.
///
/// Not CoreML: with this model every CoreML configuration that ran on the
/// GPU produced wrong stems (SNR vs CPU 0–20 dB), and the neural-network
/// format failed to compile. WebGPU matches the CPU output (75–88 dB SNR).
pub struct Separator {
    workers: Vec<Worker>,
}

fn build_session(model: &Path, threads: usize, gpu: bool) -> Result<Session, String> {
    let fail = |e: &dyn std::fmt::Display| format!("couldn't load stem model: {e}");
    let mut builder = Session::builder()
        .map_err(|e| fail(&e))?
        .with_intra_threads(threads.max(1))
        .map_err(|e| fail(&e))?
        .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
        .map_err(|e| fail(&e))?;
    if gpu {
        builder = builder
            .with_execution_providers([ort::ep::WebGPU::default().build().error_on_failure()])
            .map_err(|e| fail(&e))?;
    }
    builder.commit_from_file(model).map_err(|e| fail(&e))
}

impl Separator {
    pub fn load(model: &Path, engine: Engine) -> Result<Self, String> {
        let mut workers = Vec::new();
        if engine.gpu {
            match build_session(model, 2, true) {
                Ok(session) => workers.push(Worker { label: "gpu", session: session.into() }),
                Err(e) => eprintln!("Tempo Sampler: WebGPU unavailable, using CPU ({e})"),
            }
        }
        if engine.cpu_threads > 0 || workers.is_empty() {
            let threads = if engine.cpu_threads > 0 { engine.cpu_threads } else { Self::default_threads() };
            workers.push(Worker { label: "cpu", session: build_session(model, threads, false)?.into() });
        }
        Ok(Self { workers })
    }

    /// Devices in use, e.g. "gpu+cpu".
    pub fn devices(&self) -> String {
        self.workers.iter().map(|w| w.label).collect::<Vec<_>>().join("+")
    }

    /// CPU threads to use inside a DAW: half the cores, so separation
    /// doesn't starve the host's audio threads.
    pub fn default_threads() -> usize {
        (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).max(2)
    }

    /// Separates interleaved stereo audio. `progress` gets 0..1; returns
    /// `Ok(None)` if `cancelled()` became true.
    pub fn separate(
        &mut self,
        stereo: &[f32],
        sample_rate: u32,
        mut progress: impl FnMut(f32),
        cancelled: impl Fn() -> bool + Sync,
    ) -> Result<Option<Stems>, String> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;

        let frames_in = stereo.len() / 2;
        let x = resample_stereo(stereo, sample_rate, MODEL_SAMPLE_RATE);
        let n = x.len() / 2;

        // Normalise like the Demucs CLI: by the whole track's mono mean/std.
        let mono_sum: f64 = x.chunks(2).map(|f| (f[0] + f[1]) as f64 * 0.5).sum();
        let mean = (mono_sum / n.max(1) as f64) as f32;
        let var = x.chunks(2).map(|f| ((f[0] + f[1]) * 0.5 - mean).powi(2) as f64).sum::<f64>() / n.max(1) as f64;
        let std = (var.sqrt() as f32).max(1e-8);

        // Overlapping windows (50%), cross-faded with triangular weights.
        let stride = SEGMENT / 2;
        let weight: Vec<f32> = (0..SEGMENT).map(|i| (i + 1).min(SEGMENT - i) as f32 / stride as f32).collect();
        let offsets: Vec<usize> = (0..n.max(1)).step_by(stride).collect();
        let acc = Mutex::new((vec![vec![0.0f32; n * 2]; 4], vec![0.0f32; n]));
        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let error: Mutex<Option<String>> = Mutex::new(None);

        let run_window = |session: &Mutex<Session>, offset: usize| -> Result<(), String> {
            let len = SEGMENT.min(n - offset);
            // Planar [L..., R...], zero-padded to the window.
            let mut chunk = vec![0.0f32; 2 * SEGMENT];
            for i in 0..len {
                chunk[i] = (x[2 * (offset + i)] - mean) / std;
                chunk[SEGMENT + i] = (x[2 * (offset + i) + 1] - mean) / std;
            }
            let input = Tensor::from_array(([1usize, 2, SEGMENT], chunk)).map_err(|e| e.to_string())?;
            let mut session = session.lock().unwrap_or_else(|e| e.into_inner());
            let outputs = session.run(ort::inputs![input]).map_err(|e| e.to_string())?;
            let (_, y) = outputs[0].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
            // y: (1, 4, 2, SEGMENT)
            let mut guard = acc.lock().unwrap_or_else(|e| e.into_inner());
            let (stems, wsum) = &mut *guard;
            for s in 0..4 {
                let (l, r) = (&y[(s * 2) * SEGMENT..], &y[(s * 2 + 1) * SEGMENT..]);
                let out = &mut stems[s];
                for i in 0..len {
                    out[2 * (offset + i)] += weight[i] * l[i];
                    out[2 * (offset + i) + 1] += weight[i] * r[i];
                }
            }
            for i in 0..len {
                wsum[offset + i] += weight[i];
            }
            Ok(())
        };

        // Each device takes the next window from the queue until it's empty.
        std::thread::scope(|scope| {
            for worker in &self.workers {
                let (next, done, error, offsets, cancelled, run_window) =
                    (&next, &done, &error, &offsets, &cancelled, &run_window);
                scope.spawn(move || loop {
                    if cancelled() || error.lock().unwrap().is_some() {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&offset) = offsets.get(i) else { break };
                    if let Err(e) = run_window(&worker.session, offset) {
                        *error.lock().unwrap() = Some(e);
                        break;
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                });
            }
            // Report progress from this thread while the workers run.
            loop {
                let d = done.load(Ordering::Relaxed);
                progress(d as f32 / offsets.len() as f32);
                if d >= offsets.len() || cancelled() || error.lock().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        if let Some(e) = error.into_inner().unwrap() {
            return Err(e);
        }
        if cancelled() {
            return Ok(None);
        }

        let (acc, wsum) = acc.into_inner().unwrap();
        let stems: Vec<Vec<f32>> = acc
            .into_iter()
            .map(|mut s| {
                for (i, w) in wsum.iter().enumerate() {
                    let w = w.max(1e-8);
                    s[2 * i] = s[2 * i] / w * std + mean / 4.0;
                    s[2 * i + 1] = s[2 * i + 1] / w * std + mean / 4.0;
                }
                let mut back = resample_stereo(&s, MODEL_SAMPLE_RATE, sample_rate);
                back.resize(frames_in * 2, 0.0);
                back
            })
            .collect();
        let stems: [Vec<f32>; 4] = stems.try_into().map_err(|_| "stem count".to_string())?;
        Ok(Some(Stems { sample_rate, stems }))
    }
}

// ---------------------------------------------------------------------------
// Model location and the shared separator
// ---------------------------------------------------------------------------

/// File name of the model inside the plugin bundle's `Contents/Resources`.
pub const MODEL_FILE: &str = "htdemucs.onnx";

/// The model: `TEMPO_SAMPLER_MODEL` if set (tests, development), otherwise
/// `Contents/Resources/htdemucs.onnx` inside the plugin bundle.
pub fn model_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("TEMPO_SAMPLER_MODEL") {
        return Some(p.into());
    }
    let binary = own_binary_path()?; // .../Tempo Sampler.vst3/Contents/MacOS/Tempo Sampler
    let path = binary.parent()?.parent()?.join("Resources").join(MODEL_FILE);
    path.exists().then_some(path)
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

/// Engine used inside the plugin (decided by benchmark; see README).
pub const PLUGIN_ENGINE: Engine = Engine { gpu: true, cpu_threads: 4 };

/// One separator for the whole process, shared by every plugin instance
/// (the model needs ~1 GB of RAM while running), loaded on first use.
static SEPARATOR: std::sync::OnceLock<std::sync::Mutex<Option<Separator>>> = std::sync::OnceLock::new();

/// Runs `f` with the shared separator, loading the model first if needed.
/// Only one separation runs at a time; other callers wait.
pub fn with_separator<R>(f: impl FnOnce(&mut Separator) -> R) -> Result<R, String> {
    let cell = SEPARATOR.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        let path = model_path().ok_or("Stem model not found in the plugin bundle")?;
        *guard = Some(Separator::load(&path, PLUGIN_ENGINE)?);
    }
    Ok(f(guard.as_mut().expect("loaded above")))
}

// ---------------------------------------------------------------------------
// Disk cache
// ---------------------------------------------------------------------------

/// Stems are kept here as 16-bit WAVs, one folder per source file.
pub fn cache_root() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("TEMPO_SAMPLER_STEM_CACHE") {
        return p.into();
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    home.join("Library/Caches/Tempo Sampler/stems")
}

/// Oldest-used stems are deleted once the cache grows past this.
pub const CACHE_LIMIT_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Cache folder for a source file, keyed by path, size and modification
/// time (so an edited file is split again).
pub fn cache_dir_for(source: &Path) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    let meta = std::fs::metadata(source).ok()?;
    let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (source, meta.len(), modified, MODEL_FILE).hash(&mut h);
    Some(cache_root().join(format!("{:016x}", h.finish())))
}

/// Loads cached stems, or `None` if they aren't all there.
pub fn load_cached(dir: &Path) -> Option<Stems> {
    let mut stems: Vec<Vec<f32>> = Vec::new();
    let mut sample_rate = 0;
    for name in STEM_NAMES {
        let reader = hound::WavReader::open(dir.join(format!("{name}.wav"))).ok()?;
        let (s, sr) = crate::loops::decode_wav(reader).ok()?;
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
    for (name, s) in STEM_NAMES.iter().zip(&stems.stems) {
        let mut w = hound::WavWriter::create(tmp.join(format!("{name}.wav")), spec).map_err(|e| e.to_string())?;
        for v in s {
            w.write_sample((v.clamp(-1.0, 1.0) * 32767.0) as i16).map_err(|e| e.to_string())?;
        }
        w.finalize().map_err(|e| e.to_string())?;
    }
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
    let (start, end) = (start.min(frames), end.min(frames));
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

    fn model() -> Option<std::path::PathBuf> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("models/htdemucs_fp16weights.onnx");
        p.exists().then_some(p)
    }

    /// Needs the model in models/ (it isn't in git); skipped otherwise.
    #[test]
    fn separates_drum_loop_into_drums() {
        let Some(path) = model() else {
            eprintln!("skipping: models/htdemucs_fp16weights.onnx not present");
            return;
        };
        let a = crate::loops::LoopAsset::test_loop();
        // Two copies so the model sees more than one window.
        let x: Vec<f32> = a.samples.iter().chain(&a.samples).copied().collect();
        let mut sep = Separator::load(&path, Engine { gpu: false, cpu_threads: 4 }).unwrap();
        let st = sep.separate(&x, a.sample_rate, |_| {}, || false).unwrap().unwrap();
        assert_eq!(st.sample_rate, 48_000);
        let energy = |v: &[f32]| v.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
        let e: Vec<f64> = st.stems.iter().map(|s| energy(s)).collect();
        let total: f64 = e.iter().sum();
        assert!(e[0] / total > 0.5, "drums share {:.2}", e[0] / total);
        assert!(e[3] / total < 0.05, "vocals share {:.2}", e[3] / total);
        for s in &st.stems {
            assert_eq!(s.len(), x.len());
        }

        // Cache round trip (16-bit).
        let dir = std::env::temp_dir().join(format!("tempo-sampler-stems-{}", std::process::id()));
        save_cached(&dir, &st).unwrap();
        let back = load_cached(&dir).unwrap();
        let err = back.stems[0].iter().zip(&st.stems[0]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(err < 1e-3, "cache error {err}");
        let _ = std::fs::remove_dir_all(&dir);

        // The GPU (WebGPU) gives the same stems as the CPU. This is the check
        // CoreML failed: its GPU stems were off by 0-20 dB SNR.
        let mut gpu = Separator::load(&path, Engine { gpu: true, cpu_threads: 0 }).unwrap();
        if gpu.devices() == "gpu" {
            let g = gpu.separate(&x, a.sample_rate, |_| {}, || false).unwrap().unwrap();
            for (k, (c, gs)) in st.stems.iter().zip(&g.stems).enumerate() {
                let err: f64 = c.iter().zip(gs).map(|(a, b)| ((a - b) as f64).powi(2)).sum();
                let snr = 10.0 * (energy(c) / err.max(1e-20)).log10();
                assert!(snr > 40.0, "stem {} GPU vs CPU SNR {snr:.1} dB", STEM_NAMES[k]);
            }
        } else {
            eprintln!("WebGPU unavailable here; skipped GPU check");
        }

        // Mixing all stems gets back (close to) the original.
        let all = mix(&st, 0b1111, 0, x.len() / 2);
        let res: f64 = all.iter().zip(&x).map(|(a, b)| ((a - b) as f64).powi(2)).sum();
        assert!(10.0 * (res / energy(&x)).log10() < -10.0);
    }
}
