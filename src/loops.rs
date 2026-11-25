//! Loop assets and pitch-preserving tempo stretching.
//!
//! Nothing in here is realtime-safe (it allocates and does FFTs), so it only
//! ever runs on the background thread or in the GUI, never in `process()`.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A loop or song as stored on disk, at its original tempo. Always stereo.
pub struct LoopAsset {
    /// Library key (file path, or the built-in key).
    pub key: String,
    pub name: String,
    /// Tempo the loop was recorded at.
    pub bpm: f32,
    /// Length in quarter-note beats (whole beats for loops).
    pub beats: f32,
    /// Whole bars that should repeat seamlessly; otherwise a song or chop.
    pub is_loop: bool,
    pub sample_rate: u32,
    /// Interleaved stereo samples (L, R, L, R, ...). Shared with the decoded
    /// file when this is a whole file, so selecting a song doesn't copy it.
    pub samples: Arc<Vec<f32>>,
}

/// A loop stretched to a particular project tempo. Same sample rate and beat
/// length as its source, just a different number of frames.
pub struct RenderedLoop {
    pub key: String,
    pub name: String,
    pub bpm: f32,
    pub beats: f32,
    pub is_loop: bool,
    /// Where this audio sits in the selected song, as fractions (0..1) of
    /// the song's length: the whole song, or one chop. For the playhead.
    pub span: Option<(f32, f32)>,
    pub sample_rate: u32,
    /// Interleaved stereo samples.
    pub samples: Vec<f32>,
    /// Only the start of a song, published so it can play while the rest is
    /// still stretching. Never exported.
    pub partial: bool,
    /// `key_hash(key)`, so the audio thread can check it's the wanted clip.
    pub key_hash: u64,
}

/// A non-zero hash of a render key (0 means "any clip" to the audio thread).
pub fn key_hash(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish() | 1
}

impl LoopAsset {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    /// The built-in test loop, compiled into the plugin binary.
    pub fn test_loop() -> Self {
        let bytes = include_bytes!("../assets/test_loop_120bpm.wav");
        let (samples, sample_rate) = decode_wav(hound::WavReader::new(Cursor::new(&bytes[..])).unwrap())
            .expect("embedded test loop is valid");
        Self::new(crate::library::BUILTIN_KEY, "Test Loop", 120.0, sample_rate, samples)
    }

    /// Decodes an audio file (any supported format) from disk.
    pub fn from_file(path: &Path, key: &str, name: &str, bpm: f32) -> Result<Self, String> {
        let (samples, sample_rate) = crate::audio_file::decode(path)?;
        Ok(Self::new(key, name, bpm, sample_rate, samples))
    }

    pub fn new(key: &str, name: &str, bpm: f32, sample_rate: u32, samples: impl Into<Arc<Vec<f32>>>) -> Self {
        let samples = samples.into();
        let secs = (samples.len() / 2) as f32 / sample_rate as f32;
        let is_loop = crate::bpm::is_loop(secs, bpm);
        let beats = secs * bpm / 60.0;
        Self {
            key: key.to_string(),
            name: name.to_string(),
            bpm,
            // Loops are whole beats; round off small trimming errors.
            beats: if is_loop { beats.round().max(1.0) } else { beats },
            is_loop,
            sample_rate,
            samples,
        }
    }
}

/// Decodes a WAV held in memory (read in one go, which is much faster than
/// hound's per-sample reader): the header via hound, then the sample bytes
/// converted in bulk with the same arithmetic as `decode_wav`. Falls back
/// to `decode_wav` for packings it doesn't handle.
pub fn decode_wav_bytes(bytes: &[u8]) -> Result<(Vec<f32>, u32), hound::Error> {
    let reader = hound::WavReader::new(Cursor::new(bytes))?;
    let spec = reader.spec();
    let count = reader.len() as usize;
    let start = reader.into_inner().position() as usize;
    // The data chunk's size is the 4 bytes just before its samples.
    let data_len = (start >= 4).then(|| u32::from_le_bytes(bytes[start - 4..start].try_into().unwrap()) as usize);
    let width = match data_len {
        Some(len) if count > 0 => len / count,
        _ => 0,
    };
    let data = &bytes[start.min(bytes.len())..(start + count * width).min(bytes.len())];
    let raw: Vec<f32> = match (spec.sample_format, width, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 4, 32) => {
            data.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
        }
        (hound::SampleFormat::Int, w, bits @ (8 | 16 | 24 | 32)) if matches!((w, bits), (1, 8) | (2, 16) | (3, 24) | (4, 24) | (4, 32)) => {
            let scale = 1.0 / (1i64 << (bits - 1)) as f32;
            match (w, bits) {
                (1, _) => data.iter().map(|&x| ((x as i16 - 128) as i8) as i32 as f32 * scale).collect(),
                (2, _) => data.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as i32 as f32 * scale).collect(),
                (3, _) => data
                    .chunks_exact(3)
                    .map(|b| {
                        let x = u32::from_le_bytes([b[0], b[1], b[2], 0]);
                        let v = if x & (1 << 23) == 0 { x as i32 } else { (x | 0xff00_0000) as i32 };
                        v as f32 * scale
                    })
                    .collect(),
                (4, 24) => data
                    .chunks_exact(4)
                    .map(|b| {
                        let x = u32::from_le_bytes(b.try_into().unwrap());
                        let v = if x & (1 << 23) == 0 { (x & 0x00ff_ffff) as i32 } else { (x | 0xff00_0000) as i32 };
                        v as f32 * scale
                    })
                    .collect(),
                _ => data.chunks_exact(4).map(|b| i32::from_le_bytes(b.try_into().unwrap()) as f32 * scale).collect(),
            }
        }
        _ => return decode_wav(hound::WavReader::new(Cursor::new(bytes))?),
    };
    if raw.len() != count {
        return decode_wav(hound::WavReader::new(Cursor::new(bytes))?); // truncated file: let hound decide
    }
    Ok((to_stereo(raw, spec.channels), spec.sample_rate))
}

/// Interleaved frames of `channels` channels to interleaved stereo (mono
/// duplicated, channels beyond two dropped).
fn to_stereo(raw: Vec<f32>, channels: u16) -> Vec<f32> {
    let ch = (channels as usize).max(1);
    if ch == 2 {
        return raw;
    }
    let mut samples = Vec::with_capacity(raw.len() / ch * 2);
    for f in raw.chunks_exact(ch) {
        samples.extend_from_slice(&[f[0], if ch > 1 { f[1] } else { f[0] }]);
    }
    samples
}

/// Decodes any PCM/float WAV to interleaved stereo f32 (mono is duplicated,
/// channels beyond two are dropped). Returns (samples, sample rate).
pub fn decode_wav<R: std::io::Read>(
    reader: hound::WavReader<R>,
) -> Result<(Vec<f32>, u32), hound::Error> {
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
    Ok((to_stereo(raw, spec.channels), spec.sample_rate))
}

impl LoopAsset {

    /// Stretches to `target_bpm` without changing pitch.
    ///
    /// Loops come out seamless: the loop is stretched with a short
    /// wrap-around pad on each side (its own end before it, its own start
    /// after it), and the pads are cut off. The stretcher's start-up and tail
    /// land in the pads, and the seam is processed like any other point.
    /// Songs are stretched once, start to end.
    pub fn stretch_to(&self, target_bpm: f32) -> RenderedLoop {
        let in_frames = self.frames();
        let ratio = self.bpm as f64 / target_bpm as f64;
        let out_frames = (in_frames as f64 * ratio).round() as usize;
        let mut stretch = signalsmith_stretch::Stretch::preset_default(2, self.sample_rate);

        let samples = if self.is_loop {
            // Half a second of context each side: several times the
            // stretcher's ~0.12 s window. A loop shorter than that wraps once.
            let pad = (self.sample_rate as usize / 2).min(in_frames);
            let mut padded = Vec::with_capacity((in_frames + 2 * pad) * 2);
            padded.extend_from_slice(&self.samples[(in_frames - pad) * 2..]);
            padded.extend_from_slice(&self.samples);
            padded.extend_from_slice(&self.samples[..pad * 2]);
            let pad_out = (pad as f64 * ratio).round() as usize;
            let mut out = vec![0.0f32; (out_frames + 2 * pad_out) * 2];
            stretch.exact(&padded, &mut out);
            out.copy_within(pad_out * 2..(pad_out + out_frames) * 2, 0);
            out.truncate(out_frames * 2);
            out
        } else {
            self.stretch_stream(out_frames, None, &|| false).expect("not cancelled")
        };
        self.rendered(target_bpm, samples, false)
    }

    /// `stretch_to` for songs, giving up (None) as soon as `cancelled()`
    /// says the result is no longer wanted.
    pub fn stretch_to_cancellable(&self, target_bpm: f32, cancelled: &dyn Fn() -> bool) -> Option<RenderedLoop> {
        if self.is_loop {
            return Some(self.stretch_to(target_bpm));
        }
        let out_frames = (self.frames() as f64 * self.bpm as f64 / target_bpm as f64).round() as usize;
        let samples = self.stretch_stream(out_frames, None, cancelled)?;
        Some(self.rendered(target_bpm, samples, false))
    }

    /// The first `in_frames` of a song stretched to `target_bpm`, to start
    /// playing while the whole song stretches. Stretches a little more than
    /// asked and keeps the start, so it matches the full render (the
    /// stretcher only looks a short way ahead).
    pub fn stretch_start(&self, target_bpm: f32, in_frames: usize) -> RenderedLoop {
        let ratio = self.bpm as f64 / target_bpm as f64;
        let out_frames = (self.frames() as f64 * ratio).round() as usize;
        let keep = ((in_frames as f64 * ratio).round() as usize).min(out_frames);
        let samples = self.stretch_stream(out_frames, Some(keep), &|| false).expect("not cancelled");
        self.rendered(target_bpm, samples, true)
    }

    /// What `Stretch::exact` does (prime with the first input, process it
    /// all, undo the output latency, flush the tail), but processing in
    /// fixed-size pieces whose input positions depend only on the rate.
    /// So the first pieces are the same whatever happens later: `keep`
    /// stops early and returns just the first `keep` frames, bit-identical
    /// to the start of the full render, and `cancelled` is checked between
    /// pieces.
    fn stretch_stream(&self, out_frames: usize, keep: Option<usize>, cancelled: &dyn Fn() -> bool) -> Option<Vec<f32>> {
        const PIECE: usize = 16_384;
        let in_frames = self.frames();
        let mut st = signalsmith_stretch::Stretch::preset_default(2, self.sample_rate);
        let (lat_in, lat_out) = (st.input_latency(), st.output_latency());
        if out_frames < lat_out * 2 || in_frames <= lat_in {
            // Too short to split up (never a song): one go.
            let mut out = vec![0.0f32; out_frames * 2];
            st.exact(&self.samples[..], &mut out);
            out.truncate(keep.unwrap_or(out_frames) * 2);
            return Some(out);
        }
        let rate = in_frames as f64 / out_frames as f64;
        st.seek(&self.samples[..lat_in * 2], rate);

        // The input after the priming part, then silence, `in_frames` long.
        let real = in_frames - lat_in;
        let mut pad = Vec::new();

        // Output frames to produce before undoing the latency.
        let need = match keep {
            Some(k) => (lat_out + k).min(out_frames),
            None => out_frames,
        };
        let produced = (need.div_ceil(PIECE) * PIECE).min(out_frames);
        let mut out = vec![0.0f32; produced * 2];
        let (mut o, mut i) = (0, 0);
        while o < produced {
            if cancelled() {
                return None;
            }
            let o1 = (o + PIECE).min(out_frames);
            let i1 = ((o1 as f64 * rate).round() as usize).min(in_frames);
            if i1 <= real {
                st.process(&self.samples[(lat_in + i) * 2..(lat_in + i1) * 2], &mut out[o * 2..o1 * 2]);
            } else {
                pad.clear();
                pad.extend_from_slice(&self.samples[(lat_in + i.min(real)) * 2..]);
                pad.resize((i1 - i) * 2, 0.0);
                st.process(&pad, &mut out[o * 2..o1 * 2]);
            }
            (o, i) = (o1, i1);
        }

        // Fold the first output back onto itself, then shift by the latency.
        for k in 0..(out_frames - lat_out).min(lat_out) {
            for c in 0..2 {
                out[(k + lat_out) * 2 + c] -= out[(lat_out - 1 - k) * 2 + c];
            }
        }
        out.copy_within(lat_out * 2.., 0);
        match keep {
            Some(k) => {
                out.truncate(k * 2);
            }
            None => {
                // The last `lat_out` frames come from flushing.
                st.flush(&mut out[(out_frames - lat_out) * 2..]);
            }
        }
        Some(out)
    }

    fn rendered(&self, target_bpm: f32, samples: Vec<f32>, partial: bool) -> RenderedLoop {
        RenderedLoop {
            key: self.key.clone(),
            name: self.name.clone(),
            bpm: target_bpm,
            beats: self.beats,
            is_loop: self.is_loop,
            span: None,
            sample_rate: self.sample_rate,
            samples,
            partial,
            key_hash: key_hash(&self.key),
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
        // Samples packed to 24-bit in one buffer and written in one go
        // (same values hound's `write_sample` would write, ~10× faster).
        let max = ((1 << 23) - 1) as f32;
        let mut data = Vec::with_capacity(self.samples.len() * 3);
        for s in &self.samples {
            let v = (s.clamp(-1.0, 1.0) * max) as i32;
            data.extend_from_slice(&v.to_le_bytes()[..3]);
        }
        write_wav_pcm(&path, spec, &data, &self.tempo_chunks())?;
        Ok(path)
    }

    /// The chunks that tell a DAW what this file is, as sample packs carry:
    /// `acid` (tempo, length in beats, 4/4; how Ableton, FL and others know
    /// the tempo of a dropped file) and, for loops, `smpl` with a loop over
    /// the whole file.
    pub fn tempo_chunks(&self) -> Vec<([u8; 4], Vec<u8>)> {
        let mut acid = Vec::with_capacity(24);
        // Flags: root note set (0x02), stretch (0x04); one-shot (0x01) off.
        acid.extend_from_slice(&(0x02u32 | 0x04).to_le_bytes());
        acid.extend_from_slice(&60u16.to_le_bytes()); // root note C4
        acid.extend_from_slice(&0x8000u16.to_le_bytes()); // unknown, as written by ACID
        acid.extend_from_slice(&0f32.to_le_bytes()); // unknown
        acid.extend_from_slice(&(self.beats.round().max(1.0) as u32).to_le_bytes());
        acid.extend_from_slice(&4u16.to_le_bytes()); // meter denominator
        acid.extend_from_slice(&4u16.to_le_bytes()); // meter numerator
        acid.extend_from_slice(&self.bpm.to_le_bytes());
        let mut chunks = vec![(*b"acid", acid)];
        if self.is_loop {
            let mut smpl = Vec::with_capacity(60);
            for v in [
                0u32,                                     // manufacturer
                0,                                        // product
                (1e9 / self.sample_rate as f64) as u32,   // sample period (ns)
                60,                                       // MIDI unity note
                0,                                        // pitch fraction
                0,                                        // SMPTE format
                0,                                        // SMPTE offset
                1,                                        // loops
                0,                                        // sampler data
                0,                                        // loop: cue id
                0,                                        // forward
                0,                                        // start frame
                self.frames().saturating_sub(1) as u32,   // end frame (inclusive)
                0,                                        // fraction
                0,                                        // play count (forever)
            ] {
                smpl.extend_from_slice(&v.to_le_bytes());
            }
            chunks.push((*b"smpl", smpl));
        }
        chunks
    }
}

/// Writes a plain integer PCM WAV (`spec.bits_per_sample`, packed): RIFF
/// header, `fmt ` chunk, any `extra` chunks, then `data` (already
/// little-endian samples).
pub fn write_wav_pcm(path: &Path, spec: hound::WavSpec, data: &[u8], extra: &[([u8; 4], Vec<u8>)]) -> std::io::Result<()> {
    let ch = spec.channels as u32;
    let bytes = spec.bits_per_sample as u32 / 8;
    let block = ch * bytes;
    let extra_len: u32 = extra.iter().map(|(_, c)| 8 + c.len() as u32 + (c.len() as u32 & 1)).sum();
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + extra_len + data.len() as u32).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&(ch as u16).to_le_bytes())?;
    f.write_all(&spec.sample_rate.to_le_bytes())?;
    f.write_all(&(spec.sample_rate * block).to_le_bytes())?;
    f.write_all(&(block as u16).to_le_bytes())?;
    f.write_all(&spec.bits_per_sample.to_le_bytes())?;
    for (id, chunk) in extra {
        f.write_all(id)?;
        f.write_all(&(chunk.len() as u32).to_le_bytes())?;
        f.write_all(chunk)?;
        if chunk.len() % 2 == 1 {
            f.write_all(&[0])?; // chunks are word-aligned
        }
    }
    f.write_all(b"data")?;
    f.write_all(&(data.len() as u32).to_le_bytes())?;
    f.write_all(data)?;
    f.flush()
}

/// Where exported WAVs go. Persistent (not a cache folder), because Ableton
/// keeps referencing the file after you drop it into a set.
pub fn export_dir() -> PathBuf {
    // Tests write somewhere temporary instead.
    if let Some(p) = std::env::var_os("TEMPO_SAMPLER_EXPORT_DIR") {
        return p.into();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    // New exports only: files already in the old "Tempo Sampler" folder
    // stay there, because Ableton sets point at them.
    home.join("Music").join(crate::library::APP_NAME)
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
        let a = LoopAsset::new("sine", "sine", 120.0, sr as u32, samples);
        assert!(a.is_loop);
        let r = a.stretch_to(80.0);
        assert_eq!(r.frames(), 3 * sr);

        // Count rising zero crossings on the left channel over the middle second.
        let left: Vec<f32> = r.samples.iter().step_by(2).copied().collect();
        let mid = &left[sr..2 * sr];
        let crossings = mid.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!((crossings as f32 - 440.0).abs() < 5.0, "measured {crossings} Hz, want 440");
    }

    #[test]
    fn songs_stretch_start_to_end() {
        // 2.5 bars isn't a loop: stretched once, length scales with tempo.
        let base = LoopAsset::test_loop();
        let five_secs: Vec<f32> = base.samples.iter().chain(base.samples.iter()).take(5 * 48_000 * 2).copied().collect();
        let a = LoopAsset::new("chop", "chop", 120.0, 48_000, five_secs);
        assert!(!a.is_loop);
        assert_eq!(a.beats, 10.0);
        let r = a.stretch_to(100.0);
        assert!(!r.is_loop);
        assert_eq!(r.frames(), 6 * 48_000);
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

    /// A song's quick start render is bit-identical to the start of its full
    /// render, so swapping one for the other mid-playback is seamless.
    #[test]
    fn start_render_matches_full_render() {
        let base = LoopAsset::test_loop();
        let long: Vec<f32> = (0..10).flat_map(|_| base.samples.iter().copied()).collect(); // 40 s
        let song = LoopAsset::new("k", "song", 97.0, base.sample_rate, long);
        assert!(!song.is_loop || song.frames() > 0);
        let full = song.stretch_to(120.0);
        let start = song.stretch_start(120.0, 10 * 48_000);
        assert!(start.partial && !full.partial);
        let n = start.samples.len();
        assert_eq!(n, ((10.0 * 48_000.0 * 97.0 / 120.0) as f64).round() as usize * 2);
        let first = start.samples.iter().zip(&full.samples[..n]).position(|(a, b)| a != b);
        assert_eq!(first, None, "start render differs from frame {:?} of {}", first.map(|i| i / 2), n / 2);

        // And the piecewise full render sounds like a one-go `exact` render.
        let mut one_go = vec![0.0f32; full.samples.len()];
        signalsmith_stretch::Stretch::preset_default(2, 48_000).exact(&song.samples[..], &mut one_go);
        let energy = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>();
        let (e1, e2) = (energy(&full.samples), energy(&one_go));
        assert!((e1 / e2 - 1.0).abs() < 0.02, "energy {e1} vs {e2}");
    }

    /// Exports read back as the same 24-bit values hound would have written.
    #[test]
    fn wav_export_reads_back() {
        let r = LoopAsset::test_loop().stretch_to(100.0);
        let dir = std::env::temp_dir().join(format!("tempo-sampler-wav-{}", std::process::id()));
        let path = r.write_wav(&dir).unwrap();
        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().bits_per_sample, 24);
        assert_eq!(reader.spec().sample_rate, r.sample_rate);
        let max = ((1 << 23) - 1) as f32;
        let read: Vec<i32> = reader.into_samples::<i32>().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), r.samples.len());
        assert!(read.iter().zip(&r.samples).all(|(&v, s)| v == (s.clamp(-1.0, 1.0) * max) as i32));

        // And carries its tempo and loop for the DAW.
        let bytes = std::fs::read(&path).unwrap();
        let find = |id: &[u8]| bytes.windows(4).position(|w| w == id).map(|i| &bytes[i + 8..]);
        let acid = find(b"acid").expect("acid chunk");
        assert_eq!(u32::from_le_bytes(acid[12..16].try_into().unwrap()), 8, "beats");
        assert_eq!(f32::from_le_bytes(acid[20..24].try_into().unwrap()), 100.0, "tempo");
        let smpl = find(b"smpl").expect("smpl chunk");
        assert_eq!(u32::from_le_bytes(smpl[28..32].try_into().unwrap()), 1, "one loop");
        assert_eq!(u32::from_le_bytes(smpl[48..52].try_into().unwrap()) as usize, r.frames() - 1, "loop end");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The in-memory WAV decoder gives exactly what hound's reader gives.
    #[test]
    fn fast_wav_decode_matches_hound() {
        let a = LoopAsset::test_loop();
        let cases = [
            (1u16, 16u16, hound::SampleFormat::Int),
            (2, 16, hound::SampleFormat::Int),
            (2, 24, hound::SampleFormat::Int),
            (2, 32, hound::SampleFormat::Int),
            (2, 32, hound::SampleFormat::Float),
            (2, 8, hound::SampleFormat::Int),
        ];
        for (channels, bits, format) in cases {
            let spec = hound::WavSpec { channels, sample_rate: 44_100, bits_per_sample: bits, sample_format: format };
            let mut bytes = Cursor::new(Vec::new());
            let mut w = hound::WavWriter::new(&mut bytes, spec).unwrap();
            let max = ((1i64 << (bits - 1)) - 1) as f32;
            for f in a.samples.chunks(2).take(20_000) {
                for c in 0..channels as usize {
                    match format {
                        hound::SampleFormat::Float => w.write_sample(f[c]).unwrap(),
                        hound::SampleFormat::Int => w.write_sample((f[c] * max) as i32).unwrap(),
                    }
                }
            }
            w.finalize().unwrap();
            let bytes = bytes.into_inner();
            let slow = decode_wav(hound::WavReader::new(Cursor::new(&bytes[..])).unwrap()).unwrap();
            let fast = decode_wav_bytes(&bytes).unwrap();
            assert_eq!(fast.1, slow.1);
            assert!(fast.0 == slow.0, "{channels} ch {bits}-bit {format:?} differs");
        }
    }

    #[test]
    fn file_names() {
        let mk = |bpm| RenderedLoop {
            key: "k".into(),
            name: "Test Loop".into(),
            bpm,
            beats: 8.0,
            is_loop: true,
            span: None,
            sample_rate: 48_000,
            samples: vec![],
            partial: false,
            key_hash: 0,
        };
        assert_eq!(mk(71.0).file_name(), "Test Loop - 71 BPM.wav");
        assert_eq!(mk(92.5).file_name(), "Test Loop - 92.5 BPM.wav");
    }
}
