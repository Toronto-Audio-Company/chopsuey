//! Loop assets and pitch-preserving tempo stretching.
//!
//! Nothing in here is realtime-safe (it allocates and does FFTs), so it only
//! ever runs on the background thread or in the GUI, never in `process()`.

use std::io::Cursor;
use std::path::{Path, PathBuf};

/// A loop as stored on disk, at its original tempo. Always stereo.
pub struct LoopAsset {
    pub name: String,
    /// Tempo the loop was recorded at.
    pub bpm: f32,
    /// Length in quarter-note beats.
    pub beats: f32,
    pub sample_rate: u32,
    /// Interleaved stereo samples (L, R, L, R, ...).
    pub samples: Vec<f32>,
}

/// A loop stretched to a particular project tempo. Same sample rate and beat
/// length as its source, just a different number of frames.
pub struct RenderedLoop {
    pub name: String,
    pub bpm: f32,
    pub beats: f32,
    pub sample_rate: u32,
    /// Interleaved stereo samples.
    pub samples: Vec<f32>,
}

impl LoopAsset {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    /// The built-in test loop, compiled into the plugin binary.
    pub fn test_loop() -> Self {
        let bytes = include_bytes!("../assets/test_loop_120bpm.wav");
        Self::from_wav_bytes("Test Loop", 120.0, bytes).expect("embedded test loop is valid")
    }

    /// Decodes a WAV file. The beat length is derived from the duration and `bpm`.
    pub fn from_wav_bytes(name: &str, bpm: f32, bytes: &[u8]) -> Result<Self, hound::Error> {
        let reader = hound::WavReader::new(Cursor::new(bytes))?;
        let spec = reader.spec();
        let raw: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader.into_samples::<f32>().collect::<Result<_, _>>()?,
            hound::SampleFormat::Int => {
                let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .into_samples::<i32>()
                    .map(|s| s.map(|s| s as f32 * scale))
                    .collect::<Result<_, _>>()?
            }
        };
        let ch = spec.channels as usize;
        let samples: Vec<f32> = raw
            .chunks_exact(ch)
            .flat_map(|f| {
                let l = f[0];
                let r = if ch > 1 { f[1] } else { f[0] };
                [l, r]
            })
            .collect();
        let secs = (samples.len() / 2) as f32 / spec.sample_rate as f32;
        Ok(Self {
            name: name.to_string(),
            bpm,
            beats: (secs * bpm / 60.0).round(),
            sample_rate: spec.sample_rate,
            samples,
        })
    }

    /// Stretches the loop to `target_bpm` without changing its pitch.
    ///
    /// The result loops seamlessly: we stretch three back-to-back copies and
    /// keep the middle one, so the stretcher's start-up and tail artifacts land
    /// in the discarded copies and the seam is processed like any other point.
    pub fn stretch_to(&self, target_bpm: f32) -> RenderedLoop {
        let in_frames = self.frames();
        let out_frames = (in_frames as f64 * self.bpm as f64 / target_bpm as f64).round() as usize;

        let tiled: Vec<f32> = self.samples.repeat(3);
        let mut out = vec![0.0f32; out_frames * 3 * 2];
        let mut stretch = signalsmith_stretch::Stretch::preset_default(2, self.sample_rate);
        stretch.exact(&tiled, &mut out);

        RenderedLoop {
            name: self.name.clone(),
            bpm: target_bpm,
            beats: self.beats,
            sample_rate: self.sample_rate,
            samples: out[out_frames * 2..out_frames * 4].to_vec(),
        }
    }
}

impl RenderedLoop {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn duration_secs(&self) -> f32 {
        self.frames() as f32 / self.sample_rate as f32
    }

    /// File name like "Test Loop - 71 BPM.wav" or "Test Loop - 92.5 BPM.wav".
    pub fn file_name(&self) -> String {
        let bpm = format!("{:.2}", self.bpm);
        let bpm = bpm.trim_end_matches('0').trim_end_matches('.');
        format!("{} - {bpm} BPM.wav", self.name)
    }

    /// Writes a 24-bit stereo WAV into `dir` and returns its path.
    pub fn write_wav(&self, dir: &Path) -> Result<PathBuf, hound::Error> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(self.file_name());
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: self.sample_rate,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec)?;
        let max = ((1 << 23) - 1) as f32;
        for s in &self.samples {
            w.write_sample((s.clamp(-1.0, 1.0) * max) as i32)?;
        }
        w.finalize()?;
        Ok(path)
    }
}

/// Where exported WAVs go. Persistent (not a cache folder), because Ableton
/// keeps referencing the file after you drop it into a set.
pub fn export_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    home.join("Music").join("Tempo Sampler")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn test_loop_is_two_bars_at_120() {
        let a = LoopAsset::test_loop();
        assert_eq!(a.bpm, 120.0);
        assert_eq!(a.beats, 8.0);
        assert_eq!(a.frames(), 4 * 48_000);
    }

    #[test]
    fn stretched_length_matches_target_tempo() {
        let a = LoopAsset::test_loop();
        for bpm in [71.0, 90.0, 120.0, 140.0, 174.0] {
            let r = a.stretch_to(bpm);
            let expected_secs = a.beats * 60.0 / bpm;
            assert!(
                (r.duration_secs() - expected_secs).abs() < 1.0 / 48_000.0,
                "{bpm} BPM: got {}s, want {expected_secs}s",
                r.duration_secs()
            );
        }
    }

    #[test]
    fn stretch_keeps_pitch() {
        // 4 beats of a 440 Hz sine at 120 BPM, slowed to 80 BPM.
        let sr = 48_000;
        let samples: Vec<f32> = (0..2 * sr)
            .flat_map(|i| {
                let s = (TAU * 440.0 * i as f32 / sr as f32).sin() * 0.5;
                [s, s]
            })
            .collect();
        let a = LoopAsset { name: "sine".into(), bpm: 120.0, beats: 4.0, sample_rate: sr as u32, samples };
        let r = a.stretch_to(80.0);
        assert_eq!(r.frames(), 3 * sr);

        // Count rising zero crossings on the left channel over the middle second.
        let left: Vec<f32> = r.samples.iter().step_by(2).copied().collect();
        let mid = &left[sr..2 * sr];
        let crossings = mid.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!((crossings as f32 - 440.0).abs() < 5.0, "measured {crossings} Hz, want 440");
    }

    #[test]
    fn stretched_loop_is_not_silent() {
        let r = LoopAsset::test_loop().stretch_to(71.0);
        let rms = (r.samples.iter().map(|s| s * s).sum::<f32>() / r.samples.len() as f32).sqrt();
        assert!(rms > 0.01, "rms {rms}");
    }

    /// Writes stretched versions to target/stretch-preview/ so you can listen.
    /// Run with: cargo test --release -- --ignored write_previews
    #[test]
    #[ignore]
    fn write_previews() {
        let a = LoopAsset::test_loop();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/stretch-preview");
        for bpm in [71.0, 90.0, 140.0] {
            println!("{}", a.stretch_to(bpm).write_wav(&dir).unwrap().display());
        }
    }

    #[test]
    fn file_names() {
        let mk = |bpm| RenderedLoop { name: "Test Loop".into(), bpm, beats: 8.0, sample_rate: 48_000, samples: vec![] };
        assert_eq!(mk(71.0).file_name(), "Test Loop - 71 BPM.wav");
        assert_eq!(mk(92.5).file_name(), "Test Loop - 92.5 BPM.wav");
    }
}
