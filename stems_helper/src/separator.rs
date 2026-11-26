//! HTDemucs (4 stems) on ONNX Runtime. Lives in the helper executable, not
//! the plugin, so a crash in ONNX Runtime or the GPU driver (Dawn/WebGPU on
//! Metal) can't take the host down, and the model's ~1 GB of memory is
//! given back when a split finishes.

use std::path::Path;

use ort::session::Session;
use ort::value::Tensor;
use tempo_sampler::dsp::resample_stereo;
use tempo_sampler::safety::LockExt;
use tempo_sampler::stems::{Stems, MODEL_SAMPLE_RATE, SEGMENT};

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
                Err(e) => eprintln!("chopsuey: WebGPU unavailable, using CPU ({e})"),
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
                    if cancelled() || error.locked().is_some() {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&offset) = offsets.get(i) else { break };
                    if let Err(e) = run_window(&worker.session, offset) {
                        *error.locked() = Some(e);
                        break;
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                });
            }
            // Report progress from this thread while the workers run.
            loop {
                let d = done.load(Ordering::Relaxed);
                progress(d as f32 / offsets.len() as f32);
                if d >= offsets.len() || cancelled() || error.locked().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        if let Some(e) = error.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner) {
            return Err(e);
        }
        if cancelled() {
            return Ok(None);
        }

        let (acc, wsum) = acc.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// The engine for splits started by the plugin (decided by benchmark; see README).
pub const PLUGIN_ENGINE: Engine = Engine { gpu: true, cpu_threads: 4 };

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_sampler::stems::STEM_NAMES;

    fn model() -> Option<std::path::PathBuf> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../models/htdemucs_fp16weights.onnx");
        p.exists().then_some(p)
    }

    /// Needs the model in models/ (it isn't in git); skipped otherwise.
    #[test]
    fn separates_drum_loop_into_drums() {
        let Some(path) = model() else {
            eprintln!("skipping: models/htdemucs_fp16weights.onnx not present");
            return;
        };
        let a = tempo_sampler::loops::LoopAsset::test_loop();
        // Two copies so the model sees more than one window.
        let x: Vec<f32> = a.samples.iter().chain(a.samples.iter()).copied().collect();
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
        let all = tempo_sampler::stems::mix(&st, 0b1111, 0, x.len() / 2);
        let res: f64 = all.iter().zip(&x).map(|(a, b)| ((a - b) as f64).powi(2)).sum();
        assert!(10.0 * (res / energy(&x)).log10() < -10.0);
    }
}
