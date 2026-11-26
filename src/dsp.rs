//! Shared spectral analysis.

use crate::safety::LockExt;
use rustfft::{num_complex::Complex, FftPlanner};

/// 10 ms analysis hop.
pub fn hop_size(sample_rate: u32) -> usize {
    (sample_rate as usize / 100).max(1)
}

/// ~21 ms window: 1024 samples at 44.1/48 kHz.
pub fn fft_size(sample_rate: u32) -> usize {
    ((sample_rate as f32 * 0.021) as usize).next_power_of_two().max(256)
}

/// Runs a Hann-windowed STFT over `mono` (frame `f` starts at `f * hop`) and
/// calls `per_frame(f, log_mags)`, in frame order, with compressed
/// magnitudes `ln(1 + 100·|X|)` for bins 0..=n_fft/2. Bin `k` is
/// `k · sr / n_fft` Hz. Long inputs are split across cores (frames are
/// independent); results are identical either way.
pub fn stft_log_mags(mono: &[f32], sample_rate: u32, hop: usize, mut per_frame: impl FnMut(usize, &[f32])) {
    let n_fft = fft_size(sample_rate);
    let window: Vec<f32> = (0..n_fft)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n_fft as f32).cos())
        .collect();
    let fft = FftPlanner::<f32>::new().plan_fft_forward(n_fft);
    let bins = n_fft / 2 + 1;
    let frames = mono.len() / hop;

    // Log magnitudes of frames `range`, one after another, into `out`.
    let compute = |range: std::ops::Range<usize>, out: &mut dyn FnMut(usize, &[f32])| {
        let mut buf = vec![Complex::new(0.0f32, 0.0); n_fft];
        let mut scratch = vec![Complex::new(0.0f32, 0.0); fft.get_inplace_scratch_len()];
        let mut mag = vec![0.0f32; bins];
        for f in range {
            let start = f * hop;
            let have = mono.len().saturating_sub(start).min(n_fft);
            for ((b, &x), &w) in buf.iter_mut().zip(&mono[start..start + have]).zip(&window) {
                *b = Complex::new(x * w, 0.0);
            }
            for b in &mut buf[have..] {
                *b = Complex::new(0.0, 0.0);
            }
            fft.process_with_scratch(&mut buf, &mut scratch);
            for (m, c) in mag.iter_mut().zip(&buf[..bins]) {
                *m = (1.0 + 100.0 * c.norm()).ln();
            }
            out(f, &mag);
        }
    };

    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(12);
    if threads <= 1 || frames < PARALLEL_MIN_FRAMES {
        compute(0..frames, &mut per_frame);
        return;
    }
    let chunk = frames.div_ceil(threads);
    let results: Vec<Vec<f32>> = std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..threads)
            .map(|t| {
                let range = (t * chunk).min(frames)..((t + 1) * chunk).min(frames);
                let compute = &compute;
                scope.spawn(move || {
                    let mut out = Vec::with_capacity(range.len() * bins);
                    compute(range, &mut |_, mag| out.extend_from_slice(mag));
                    out
                })
            })
            .collect();
        // A worker that panicked contributes nothing; the panic was logged.
        jobs.into_iter().map(|j| j.join().unwrap_or_default()).collect()
    });
    let mut f = 0;
    for out in &results {
        for mag in out.chunks_exact(bins) {
            per_frame(f, mag);
            f += 1;
        }
    }
}

/// Below this many frames (~20 s) the STFT runs on the calling thread.
const PARALLEL_MIN_FRAMES: usize = 2_000;

/// Subtracts a centred moving average (±`half` frames), removing slow
/// loudness trends so only short-term changes remain.
pub fn detrend(x: &[f32], half: usize) -> Vec<f32> {
    let n = x.len();
    let mut prefix = vec![0.0f64; n + 1];
    for i in 0..n {
        prefix[i + 1] = prefix[i] + x[i] as f64;
    }
    (0..n)
        .map(|i| {
            let (a, b) = (i.saturating_sub(half), (i + half + 1).min(n));
            x[i] - ((prefix[b] - prefix[a]) / (b - a) as f64) as f32
        })
        .collect()
}

/// Resamples interleaved stereo with a windowed-sinc (Blackman) filter.
/// Polyphase table, ±32 taps (wider when downsampling), anti-aliased at
/// 95% of the lower Nyquist. Quality is well past what stem separation or
/// time-stretching can resolve.
pub fn resample_stereo(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let (half, table) = sinc_table(from, to);
    let taps = 2 * half as usize;

    let n_in = (x.len() / 2) as isize;
    let n_out = ((x.len() / 2) as f64 * ratio).round() as usize;
    let mut out = vec![0.0f32; n_out * 2];
    // Outputs are independent: split them across cores. Away from the ends
    // every tap is in range, so the inner loop needs no bounds checks (same
    // arithmetic in the same order either way, so the same result).
    let fill = |first: usize, out: &mut [f32]| {
        for (k, o) in out.chunks_exact_mut(2).enumerate() {
            let n = first + k;
            let t = n as f64 / ratio;
            let base = t.floor() as isize;
            let p = (((t - base as f64) * PHASES as f64) as usize).min(PHASES - 1);
            let row = &table[p * taps..(p + 1) * taps];
            let start = base - half + 1;
            let (mut l, mut r) = (0.0f32, 0.0f32);
            if start >= 0 && start + taps as isize <= n_in {
                let src = &x[2 * start as usize..2 * (start as usize + taps)];
                for (s, &w) in src.chunks_exact(2).zip(row) {
                    l += s[0] * w;
                    r += s[1] * w;
                }
            } else {
                for (j, &w) in row.iter().enumerate() {
                    let k = start + j as isize;
                    if k >= 0 && k < n_in {
                        l += x[2 * k as usize] * w;
                        r += x[2 * k as usize + 1] * w;
                    }
                }
            }
            o[0] = l;
            o[1] = r;
        }
    };
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(12);
    if threads <= 1 || n_out < 50_000 {
        fill(0, &mut out);
    } else {
        let chunk = n_out.div_ceil(threads);
        std::thread::scope(|scope| {
            for (c, part) in out.chunks_mut(chunk * 2).enumerate() {
                let fill = &fill;
                scope.spawn(move || fill(c * chunk, part));
            }
        });
    }
    out
}

const PHASES: usize = 2048;

/// The polyphase filter for `from` → `to` Hz: (half-width in taps, table of
/// `PHASES` rows × `2 * half` weights). Built once per rate pair.
fn sinc_table(from: u32, to: u32) -> (isize, std::sync::Arc<Vec<f32>>) {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static TABLES: OnceLock<Mutex<HashMap<(u32, u32), (isize, Arc<Vec<f32>>)>>> = OnceLock::new();
    let tables = TABLES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(t) = tables.locked().get(&(from, to)) {
        return t.clone();
    }
    let ratio = to as f64 / from as f64;
    let fc = ratio.min(1.0) * 0.95; // cutoff, relative to the input Nyquist
    let half = (32.0 / ratio.min(1.0)).ceil() as isize;
    let taps = 2 * half as usize;

    // table[p][j]: weight of input sample (floor(t) - half + 1 + j) for an
    // output at fractional input position frac = p / PHASES.
    let sinc = |x: f64| if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
    let blackman = |u: f64| {
        if u.abs() >= 1.0 {
            0.0
        } else {
            0.42 + 0.5 * (std::f64::consts::PI * u).cos() + 0.08 * (2.0 * std::f64::consts::PI * u).cos()
        }
    };
    let mut table = vec![0.0f32; PHASES * taps];
    for p in 0..PHASES {
        let frac = p as f64 / PHASES as f64;
        let row: Vec<f64> = (0..taps)
            .map(|j| {
                let d = frac + (half - 1) as f64 - j as f64;
                fc * sinc(fc * d) * blackman(d / half as f64)
            })
            .collect();
        let sum: f64 = row.iter().sum();
        for (j, v) in row.iter().enumerate() {
            table[p * taps + j] = (v / sum) as f32;
        }
    }
    let entry = (half, Arc::new(table));
    tables.locked().insert((from, to), entry.clone());
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_frequency_and_level() {
        let tone = |sr: u32, secs: f32| -> Vec<f32> {
            (0..(sr as f32 * secs) as usize)
                .flat_map(|i| {
                    let s = (std::f32::consts::TAU * 1000.0 * i as f32 / sr as f32).sin() * 0.5;
                    [s, s]
                })
                .collect()
        };
        let x = tone(48_000, 1.0);
        let y = resample_stereo(&x, 48_000, 44_100);
        assert_eq!(y.len() / 2, 44_100);
        let expected = tone(44_100, 1.0);
        // Compare away from the edges.
        let err = y[2000..86000].iter().zip(&expected[2000..86000]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(err < 0.003, "max error {err}");
        // Round trip.
        let back = resample_stereo(&y, 44_100, 48_000);
        assert_eq!(back.len(), x.len());
        let err = back[4000..90000].iter().zip(&x[4000..90000]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(err < 0.005, "round-trip max error {err}");
    }
}
