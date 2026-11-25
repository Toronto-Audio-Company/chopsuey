//! Reading audio files of any common format.
//!
//! WAV goes through `hound` (what the plugin has always used); everything
//! else through Symphonia: AIFF, FLAC, MP3, AAC / M4A, ALAC, Ogg Vorbis, CAF.
//! Exports and stem caches are always written as WAV.

use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// File extensions the library picks up (lowercase).
pub const EXTENSIONS: [&str; 11] = ["wav", "wave", "aif", "aiff", "aifc", "flac", "mp3", "m4a", "aac", "ogg", "caf"];

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| EXTENSIONS.contains(&e.as_str()))
}

fn is_wav(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav") || e.eq_ignore_ascii_case("wave"))
}

/// Length in seconds and sample rate, from the header where possible.
pub fn probe(path: &Path) -> Result<(f32, u32), String> {
    if is_wav(path) {
        let r = hound::WavReader::open(path).map_err(|e| e.to_string())?;
        let sr = r.spec().sample_rate.max(1);
        return Ok((r.duration() as f32 / sr as f32, sr));
    }
    let (format, track_id) = open(path)?;
    let track = format.tracks().iter().find(|t| t.id == track_id).ok_or("no audio track")?;
    let sr = audio_params(track)?.sample_rate.ok_or("unknown sample rate")?;
    match track.num_frames {
        Some(frames) => Ok((frames as f32 / sr as f32, sr)),
        // Some MP3s don't say how long they are: count by decoding.
        None => {
            let (samples, sr) = decode(path)?;
            Ok(((samples.len() / 2) as f32 / sr as f32, sr))
        }
    }
}

/// Decodes a whole file to interleaved stereo f32 (mono is duplicated,
/// channels beyond two are dropped). Returns (samples, sample rate).
pub fn decode(path: &Path) -> Result<(Vec<f32>, u32), String> {
    if is_wav(path) {
        // One read, then bulk conversion (several times faster than hound's
        // per-sample reader, same values).
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        return crate::loops::decode_wav_bytes(&bytes).map_err(|e| e.to_string());
    }
    let (mut format, track_id) = open(path)?;
    let track = format.tracks().iter().find(|t| t.id == track_id).ok_or("no audio track")?;
    let params = audio_params(track)?.clone();
    let sample_rate = params.sample_rate.ok_or("unknown sample rate")?;
    // Gapless: trims encoder delay/padding (MP3, AAC), which would otherwise
    // shift a loop off the beat.
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default().gapless(true))
        .map_err(|e| format!("unsupported audio: {e}"))?;

    let mut out = Vec::new();
    let mut frame = Vec::<f32>::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => return Err(format!("couldn't read audio: {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(buf) => buf,
            // A corrupt packet: skip it, like most players do.
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(format!("couldn't decode audio: {e}")),
        };
        let channels = buf.num_planes().max(1);
        buf.copy_to_vec_interleaved(&mut frame);
        out.reserve(frame.len() / channels * 2);
        for f in frame.chunks_exact(channels) {
            out.push(f[0]);
            out.push(if channels > 1 { f[1] } else { f[0] });
        }
    }
    Ok((out, sample_rate))
}

/// Just the part of a file that tempo detection looks at
/// ([`crate::bpm::analysis_window`]), as mono, plus the sample rate and the
/// file's length in seconds. Same samples as decoding the whole file and
/// cutting the window out of its mono mix, without decoding (or keeping)
/// the rest when the format allows skipping.
pub fn decode_mono_window(path: &Path) -> Result<(Vec<f32>, u32, f32), String> {
    let mono_of = |stereo: &[f32]| -> Vec<f32> { stereo.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5).collect() };
    if is_wav(path) {
        let (stereo, sr) = decode(path)?;
        let frames = stereo.len() / 2;
        let (start, len) = crate::bpm::analysis_window(frames, sr);
        return Ok((mono_of(&stereo[start * 2..(start + len) * 2]), sr, frames as f32 / sr as f32));
    }
    let (mut format, track_id) = open(path)?;
    let track = format.tracks().iter().find(|t| t.id == track_id).ok_or("no audio track")?;
    let params = audio_params(track)?.clone();
    let sample_rate = params.sample_rate.ok_or("unknown sample rate")?;
    let Some(total) = track.num_frames else {
        // Length unknown up front: decode it all.
        let (stereo, sr) = decode(path)?;
        let frames = stereo.len() / 2;
        let (start, len) = crate::bpm::analysis_window(frames, sr);
        return Ok((mono_of(&stereo[start * 2..(start + len) * 2]), sr, frames as f32 / sr as f32));
    };
    let total = total as usize;
    let (start, len) = crate::bpm::analysis_window(total, sample_rate);
    let end = start + len;
    // Timestamps count frames when the time base is 1/sample rate; then
    // packets well before the window needn't be decoded (decoding resumes
    // a second early so the decoder has settled by the window).
    let frame_stamps = track
        .time_base
        .is_some_and(|tb| tb.numer.get() == 1 && tb.denom.get() == sample_rate);
    let resume = start.saturating_sub(sample_rate as usize);
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default().gapless(true))
        .map_err(|e| format!("unsupported audio: {e}"))?;

    let mut out = Vec::with_capacity(len);
    let mut frame = Vec::<f32>::new();
    let mut pos = 0usize; // frame index of the next decoded frame
    let mut skipped = false;
    while pos < end {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => return Err(format!("couldn't read audio: {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        if frame_stamps {
            let pts = packet.pts.get().max(0) as usize;
            if pts + packet.dur.get() as usize <= resume {
                skipped = true;
                continue;
            }
            if skipped {
                decoder.reset();
                skipped = false;
            }
            pos = pts;
        }
        let buf = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(format!("couldn't decode audio: {e}")),
        };
        let channels = buf.num_planes().max(1);
        buf.copy_to_vec_interleaved(&mut frame);
        for f in frame.chunks_exact(channels) {
            if (start..end).contains(&pos) {
                let (l, r) = (f[0], if channels > 1 { f[1] } else { f[0] });
                out.push((l + r) * 0.5);
            }
            pos += 1;
        }
    }
    Ok((out, sample_rate, total as f32 / sample_rate as f32))
}

fn open(path: &Path) -> Result<(Box<dyn FormatReader>, u32), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension() {
        hint.with_extension(&ext.to_string_lossy());
    }
    let format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("unsupported file: {e}"))?;
    let track_id = format.default_track(TrackType::Audio).ok_or("no audio track")?.id;
    Ok((format, track_id))
}

fn audio_params(track: &symphonia::core::formats::Track) -> Result<&symphonia::core::codecs::audio::AudioCodecParameters, String> {
    match &track.codec_params {
        Some(CodecParameters::Audio(p)) => Ok(p),
        _ => Err("not an audio track".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::LoopAsset;

    /// Writes 16-bit big-endian stereo PCM as a minimal AIFF.
    fn write_aiff(path: &Path, stereo: &[f32], sample_rate: u32) {
        let frames = (stereo.len() / 2) as u32;
        let data: Vec<u8> = stereo
            .iter()
            .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_be_bytes())
            .collect();
        // 80-bit IEEE extended sample rate.
        let rate = {
            let r = sample_rate as f64;
            let exp = r.log2().floor() as i32;
            let mant = (r / 2f64.powi(exp) * 2f64.powi(63)) as u64;
            let mut b = [0u8; 10];
            b[..2].copy_from_slice(&((exp + 16383) as u16).to_be_bytes());
            b[2..].copy_from_slice(&mant.to_be_bytes());
            b
        };
        let mut comm = Vec::new();
        comm.extend_from_slice(&2u16.to_be_bytes());
        comm.extend_from_slice(&frames.to_be_bytes());
        comm.extend_from_slice(&16u16.to_be_bytes());
        comm.extend_from_slice(&rate);
        let mut ssnd = vec![0u8; 8]; // offset, block size
        ssnd.extend_from_slice(&data);
        let mut body = b"AIFF".to_vec();
        for (id, chunk) in [(b"COMM", &comm), (b"SSND", &ssnd)] {
            body.extend_from_slice(id);
            body.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
            body.extend_from_slice(chunk);
        }
        let mut file = b"FORM".to_vec();
        file.extend_from_slice(&(body.len() as u32).to_be_bytes());
        file.extend_from_slice(&body);
        std::fs::write(path, file).unwrap();
    }

    #[test]
    fn reads_aiff_like_wav() {
        let a = LoopAsset::test_loop();
        let dir = std::env::temp_dir().join(format!("tempo-sampler-aiff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("loop.aiff");
        write_aiff(&path, &a.samples, a.sample_rate);

        assert!(is_supported(&path));
        let (secs, sr) = probe(&path).unwrap();
        assert_eq!(sr, 48_000);
        assert!((secs - 4.0).abs() < 1e-3, "{secs}");

        let (samples, sr) = decode(&path).unwrap();
        assert_eq!(sr, 48_000);
        assert_eq!(samples.len(), a.samples.len());
        let err = samples.iter().zip(a.samples.iter()).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
        assert!(err < 1e-3, "max error {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn supported_extensions() {
        for ok in ["a.wav", "b.AIFF", "c.aif", "d.flac", "e.mp3", "f.m4a", "g.ogg"] {
            assert!(is_supported(Path::new(ok)), "{ok}");
        }
        for no in ["a.asd", "b.serato-stems", "c.txt", "d"] {
            assert!(!is_supported(Path::new(no)), "{no}");
        }
    }
}
