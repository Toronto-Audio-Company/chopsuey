//! Musical key and loudness of a sample, for the library browser.
//!
//! Key: a chromagram (spectrum folded into 12 pitch classes, 50 Hz–1 kHz)
//! correlated with major and minor key profiles (Faraldo's EDMA profiles,
//! fitted on loop-based music); the best of the 24 keys wins if it's
//! convincing. Loudness: RMS level, used to play
//! previews at matched volume.

use rustfft::{num_complex::Complex, FftPlanner};

/// Pitch-class names (flats where they're the usual spelling).
const NAMES: [&str; 12] = ["C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];

/// Krumhansl–Kessler key profiles (probe-tone ratings), tonic first.
const MAJOR: [f32; 12] = [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88];
const MINOR: [f32; 12] = [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17];

/// Below this correlation there's no clear key (drums, noise, speech).
const MIN_CONFIDENCE: f32 = 0.6;

/// How the chroma is built and matched (tunable, for `examples/key_bench`).
#[derive(Clone, Copy, Debug)]
pub struct KeyOptions {
    pub profiles: Profiles,
    pub fmin: f32,
    pub fmax: f32,
    /// Magnitude compression: 1.0 linear, 0.5 square root, 0.0 log.
    pub power: f32,
    pub min_confidence: f32,
}

#[derive(Clone, Copy, Debug)]
pub enum Profiles {
    Krumhansl,
    Temperley,
    /// Faraldo's EDMA profiles, fitted on electronic / loop-based music.
    Edma,
}

/// Chosen with `examples/key_bench` against keys written in file names:
/// EDMA profiles over 50 Hz–1 kHz (where the notes and bass lines are) got
/// 36 of 49 right, against 20 for Krumhansl over 65 Hz–5 kHz.
pub const DEFAULT_OPTIONS: KeyOptions =
    KeyOptions { profiles: Profiles::Edma, fmin: 50.0, fmax: 1000.0, power: 1.0, min_confidence: MIN_CONFIDENCE };

const TEMPERLEY_MAJOR: [f32; 12] = [5.0, 2.0, 3.5, 2.0, 4.5, 4.0, 2.0, 4.5, 2.0, 3.5, 1.5, 4.0];
const TEMPERLEY_MINOR: [f32; 12] = [5.0, 2.0, 3.5, 4.5, 2.0, 4.0, 2.0, 4.5, 3.5, 2.0, 1.5, 4.0];
const EDMA_MAJOR: [f32; 12] = [0.16519551, 0.04749026, 0.08293076, 0.06687112, 0.09994645, 0.09274123, 0.05294487, 0.13159476, 0.05218986, 0.07443653, 0.06940723, 0.06424545];
const EDMA_MINOR: [f32; 12] = [0.17235348, 0.04, 0.0761009, 0.12, 0.05621498, 0.08527853, 0.0497094, 0.13451001, 0.07458916, 0.05003023, 0.09187879, 0.05545106];

/// Estimates the key of mono audio: "Am", "F#", ... or `None` if unclear.
pub fn detect_key(mono: &[f32], sample_rate: u32) -> Option<String> {
    detect_key_with(mono, sample_rate, DEFAULT_OPTIONS)
}

pub fn detect_key_with(mono: &[f32], sample_rate: u32, opts: KeyOptions) -> Option<String> {
    let chroma = chroma(mono, sample_rate, opts)?;
    let (major, minor) = match opts.profiles {
        Profiles::Krumhansl => (MAJOR, MINOR),
        Profiles::Temperley => (TEMPERLEY_MAJOR, TEMPERLEY_MINOR),
        Profiles::Edma => (EDMA_MAJOR, EDMA_MINOR),
    };
    let mut best = (f32::MIN, String::new());
    for tonic in 0..12 {
        for (profile, suffix) in [(&major, ""), (&minor, "m")] {
            let rotated: Vec<f32> = (0..12).map(|i| profile[(i + 12 - tonic) % 12]).collect();
            let r = pearson(&chroma, &rotated);
            if r > best.0 {
                best = (r, format!("{}{suffix}", NAMES[tonic]));
            }
        }
    }
    (best.0 >= opts.min_confidence).then_some(best.1)
}

/// Average energy per pitch class over the whole input (65 Hz–5 kHz).
/// A long FFT (~6 Hz bins) so low notes land in the right class.
fn chroma(mono: &[f32], sample_rate: u32, opts: KeyOptions) -> Option<[f32; 12]> {
    let n = (sample_rate as usize / 6).next_power_of_two(); // 8192 at 44.1/48 kHz
    if mono.len() < n {
        return None;
    }
    let hop = n / 2;
    let window: Vec<f32> = (0..n).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos()).collect();
    let fft = FftPlanner::<f32>::new().plan_fft_forward(n);
    let bin_hz = sample_rate as f32 / n as f32;
    // Pitch class of each bin in range (None outside 65 Hz–5 kHz).
    let classes: Vec<Option<usize>> = (0..n / 2)
        .map(|k| {
            let f = k as f32 * bin_hz;
            (opts.fmin..opts.fmax).contains(&f).then(|| {
                let midi = 69.0 + 12.0 * (f / 440.0).log2();
                (midi.round() as i64).rem_euclid(12) as usize
            })
        })
        .collect();
    let mut buf = vec![Complex::new(0.0f32, 0.0); n];
    let mut scratch = vec![Complex::new(0.0f32, 0.0); fft.get_inplace_scratch_len()];
    let mut chroma = [0.0f32; 12];
    let mut start = 0;
    while start + n <= mono.len() {
        for ((b, &x), &w) in buf.iter_mut().zip(&mono[start..start + n]).zip(&window) {
            *b = Complex::new(x * w, 0.0);
        }
        fft.process_with_scratch(&mut buf, &mut scratch);
        for (c, class) in buf[..n / 2].iter().zip(&classes) {
            if let Some(pc) = class {
                let m = c.norm();
                chroma[*pc] += if opts.power == 1.0 {
                    m
                } else if opts.power == 0.0 {
                    (1.0 + 100.0 * m).ln()
                } else {
                    m.powf(opts.power)
                };
            }
        }
        start += hop;
    }
    let total: f32 = chroma.iter().sum();
    (total > 1e-6).then_some(chroma)
}

fn pearson(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len() as f32;
    let (ma, mb) = (a.iter().sum::<f32>() / n, b.iter().sum::<f32>() / n);
    let (mut cov, mut va, mut vb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        cov += (x - ma) * (y - mb);
        va += (x - ma) * (x - ma);
        vb += (y - mb) * (y - mb);
    }
    if va <= 0.0 || vb <= 0.0 {
        0.0
    } else {
        cov / (va.sqrt() * vb.sqrt())
    }
}

/// A key written in a file name: "F#m", "Gbmaj", "Dm", "D#", or a bare
/// letter right after a tempo ("120BPM G"). Normalised to `NAMES` spelling.
pub fn key_from_name(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let has_bpm = lower.contains("bpm");
    let tokens: Vec<&str> = name.split(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).filter(|t| !t.is_empty()).collect();
    for (i, token) in tokens.iter().enumerate() {
        let mut chars = token.chars();
        let Some(letter @ 'A'..='G') = chars.next() else { continue };
        let rest: String = chars.collect();
        let (accidental, mode) = match rest.as_bytes().first() {
            Some(b'#') => (1i32, &rest[1..]),
            Some(b'b') => (-1, &rest[1..]), // "Gb", "Gbm", "Gbmaj"
            _ => (0, &rest[..]),
        };
        let minor = match mode {
            "" => false,
            "m" | "min" | "minor" => true,
            "maj" | "major" => false,
            _ => continue,
        };
        // A lone letter is only a key next to a tempo ("120BPM G").
        let explicit = accidental != 0 || !mode.is_empty();
        let after_tempo = i > 0 && tokens[i - 1].to_lowercase().ends_with("bpm");
        if !explicit && !(has_bpm && after_tempo) {
            continue;
        }
        let natural = match letter {
            'C' => 0,
            'D' => 2,
            'E' => 4,
            'F' => 5,
            'G' => 7,
            'A' => 9,
            _ => 11,
        };
        let pc = (natural + accidental).rem_euclid(12) as usize;
        return Some(format!("{}{}", NAMES[pc], if minor { "m" } else { "" }));
    }
    None
}

/// RMS level in dBFS.
pub fn loudness_db(mono: &[f32]) -> f32 {
    if mono.is_empty() {
        return -100.0;
    }
    let ms = mono.iter().map(|x| x * x).sum::<f32>() / mono.len() as f32;
    10.0 * ms.max(1e-10).log10()
}

/// Preview gain that brings a sample at `loudness_db` to a common level
/// (−16 dBFS RMS), limited to ±12 dB.
pub fn matching_gain(loudness_db: f32) -> f32 {
    let db = (-16.0 - loudness_db).clamp(-12.0, 12.0);
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(freqs: &[f32], secs: f32) -> Vec<f32> {
        let sr = 48_000.0;
        (0..(secs * sr) as usize)
            .map(|i| freqs.iter().map(|f| (std::f32::consts::TAU * f * i as f32 / sr).sin()).sum::<f32>() * 0.2)
            .collect()
    }

    #[test]
    fn detects_major_and_minor_chords() {
        // A minor: A3 C4 E4 with the A in the bass.
        assert_eq!(detect_key(&chord(&[110.0, 220.0, 261.63, 329.63], 6.0), 48_000).as_deref(), Some("Am"));
        // C major: C3 E3 G3 C4.
        assert_eq!(detect_key(&chord(&[130.81, 164.81, 196.0, 261.63], 6.0), 48_000).as_deref(), Some("C"));
    }

    #[test]
    fn noise_has_no_key() {
        let mut x = 12345u32;
        let noise: Vec<f32> = (0..48_000 * 4)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect();
        assert_eq!(detect_key(&noise, 48_000), None);
    }

    #[test]
    fn keys_in_file_names() {
        assert_eq!(key_from_name("105BPM Dm").as_deref(), Some("Dm"));
        assert_eq!(key_from_name("105BPM F#m").as_deref(), Some("F#m"));
        assert_eq!(key_from_name("120BPM G").as_deref(), Some("G"));
        assert_eq!(key_from_name("127BPM D#(fyeloops)").as_deref(), Some("Eb"));
        assert_eq!(key_from_name("130BPM Gbmaj").as_deref(), Some("F#"));
        assert_eq!(key_from_name("140BPM Em").as_deref(), Some("Em"));
        assert_eq!(key_from_name("Air (feat. Doom) (Acapella)"), None);
        assert_eq!(key_from_name("Aretha One Step Ahead"), None);
        assert_eq!(key_from_name("Grace"), None);
    }

    #[test]
    fn loudness_and_gain() {
        let tone = chord(&[440.0], 1.0); // amplitude 0.2: RMS ≈ −17 dBFS
        let db = loudness_db(&tone);
        assert!((db + 16.99).abs() < 0.1, "{db}");
        assert!((matching_gain(db) - 1.12).abs() < 0.02);
        assert_eq!(matching_gain(-60.0), 10f32.powf(12.0 / 20.0), "limited to +12 dB");
    }
}
