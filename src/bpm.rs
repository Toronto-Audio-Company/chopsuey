//! Working out a file's original tempo: from its name, or by analysis.

/// Bump when detection changes, so cached results from an older detector
/// are thrown away and files are analysed again.
pub const DETECTOR_VERSION: u32 = 2;

/// Files up to this long are treated as possible loops (seamless, whole bars).
pub const LOOP_MAX_SECS: f32 = 60.0;

const MIN_BPM: f32 = 60.0;
const MAX_BPM: f32 = 190.0;
/// When half and double tempo fit equally well, lean towards this. 110 suits
/// soul / hip-hop / sample sources, where 80–100 BPM is common.
const PRIOR_CENTER_BPM: f32 = 110.0;
/// Long files are analysed on this much audio from the middle.
const ANALYSIS_MAX_SECS: f32 = 90.0;

/// A tempo found in a file name.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NameBpm {
    /// Next to the word "bpm", e.g. "Drums 124bpm", "BPM_90", "104 [BPM]".
    /// Trusted as-is.
    Explicit(f32),
    /// A bare number that looks like a tempo, e.g. "loop_140_Cmin". Only
    /// trusted if the file's length or the detected tempo agrees.
    Bare(f32),
}

/// Characters allowed between a number and the word "bpm".
const BPM_GAP: [char; 6] = [' ', '_', '-', '[', '(', '.'];

/// Finds a tempo in a file stem (no extension).
pub fn bpm_from_name(stem: &str) -> Option<NameBpm> {
    let s = stem.to_ascii_lowercase();
    let b = s.as_bytes();
    let is_sep = |c: u8| !c.is_ascii_alphanumeric();
    let mut first_bare = None;

    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        // Read a number like "124" or "92.5".
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
        let end = i;
        let Ok(value) = s[start..end].parse::<f32>() else { continue };

        // "124bpm", "124 bpm", "124_bpm", "104 [bpm]", "(120 bpm)"
        let after = s[end..].trim_start_matches(BPM_GAP);
        // "bpm124", "bpm 124", "bpm_124", "bpm: 124", "[bpm] 90"
        let before = s[..start].trim_end_matches([' ', '_', '-', ':', '=', ']', ')', '[', '(']);
        if (after.starts_with("bpm") || before.ends_with("bpm")) && (40.0..=300.0).contains(&value)
        {
            return Some(NameBpm::Explicit(value));
        }

        let delimited_left = start == 0 || is_sep(b[start - 1]);
        let delimited_right = end == b.len() || is_sep(b[end]);
        let integer = !s[start..end].contains('.');
        if first_bare.is_none()
            && delimited_left
            && delimited_right
            && integer
            && (60.0..=200.0).contains(&value)
        {
            first_bare = Some(NameBpm::Bare(value));
        }
    }
    first_bare
}

/// True if `secs` of audio at `bpm` is a whole number of 4/4 bars (1..=32).
pub fn is_whole_bars(secs: f32, bpm: f32) -> bool {
    let beats = secs * bpm / 60.0;
    let bars = (beats / 4.0).round();
    (1.0..=32.0).contains(&bars) && (beats - bars * 4.0).abs() <= 0.08
}

/// True if a file of this length at this tempo should be handled as a
/// seamless loop (stretched to exact bars, previewed locked to the bar grid).
pub fn is_loop(secs: f32, bpm: f32) -> bool {
    secs <= LOOP_MAX_SECS && is_whole_bars(secs, bpm)
}

/// True if `a` and `b` are within `tolerance` (fraction) of each other.
pub fn close(a: f32, b: f32, tolerance: f32) -> bool {
    (a / b).ln().abs() < tolerance
}

/// What tempo analysis found, before deciding on one value.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub secs: f32,
    /// Best tempo from rhythm alone, and its score.
    pub free: (f32, f32),
    /// For short files: tempos at which the file is exactly 1, 2, 4 ... 32
    /// bars long, with their rhythm scores.
    pub bar_candidates: Vec<(f32, f32)>,
}

/// For short files, a whole-bars tempo wins unless rhythm alone scores this
/// many times higher. Loops are usually cut to bars; this mostly matters for
/// melodic loops, where rhythm evidence is weak and the length is the best clue.
const BAR_LENGTH_TRUST: f32 = 2.0;
/// Below this rhythm score there's too little rhythm to judge by (pads,
/// melodic loops with soft attacks). Short files then go purely by length,
/// choosing between whole-bar tempos by the ~110 BPM preference.
/// Drum loops score 0.06–0.5; melodic loops 0.01–0.035 in testing.
const WEAK_RHYTHM: f32 = 0.045;

/// Estimates the tempo of mono audio of any length. See [`analyse`].
/// The classic failure is half or double the real tempo; the GUI has
/// ½× / 2× buttons for that.
pub fn detect_bpm(mono: &[f32], sample_rate: u32) -> Option<f32> {
    let a = analyse(mono, sample_rate)?;
    if a.free.1 < WEAK_RHYTHM && !a.bar_candidates.is_empty() {
        let by_prior = a
            .bar_candidates
            .iter()
            .map(|&(bpm, _)| (bpm, (bpm / PRIOR_CENTER_BPM).log2().abs()))
            .min_by(|x, y| x.1.total_cmp(&y.1));
        if let Some((bpm, _)) = by_prior {
            return Some((bpm * 100.0).round() / 100.0);
        }
    }
    let best_bar = a.bar_candidates.iter().copied().max_by(|x, y| x.1.total_cmp(&y.1));
    if let Some((bpm, score)) = best_bar {
        if close(bpm, a.free.0, 0.03) || a.free.1 <= score * BAR_LENGTH_TRUST {
            return Some((bpm * 100.0).round() / 100.0);
        }
    }
    Some((a.free.0 * 10.0).round() / 10.0)
}

/// 1. Build an onset envelope: spectral flux, i.e. how much each frequency
///    bin gets louder every 10 ms. Catches note and syllable onsets as well
///    as drum hits.
/// 2. Autocorrelate it: rhythmic audio correlates with itself shifted by one
///    beat, two beats, and so on.
/// 3. Score every tempo from 60 to 190 BPM by how well beats 1..8 line up,
///    weighted towards ~110 BPM, and refine the best one to 0.005 BPM.
/// 4. For short files, also score the tempos their length allows.
pub fn analyse(mono: &[f32], sample_rate: u32) -> Option<Analysis> {
    let secs = mono.len() as f32 / sample_rate as f32;
    if secs < 1.0 {
        return None; // a one-shot
    }
    let maybe_loop = secs <= LOOP_MAX_SECS;

    // Long files: analyse a window from the middle (intros and outros are
    // often beatless).
    let max_len = (ANALYSIS_MAX_SECS * sample_rate as f32) as usize;
    let audio = if mono.len() > max_len {
        let start = (mono.len() - max_len) / 2;
        &mono[start..start + max_len]
    } else {
        mono
    };

    let hop = hop_size(sample_rate);
    let frame_rate = sample_rate as f32 / hop as f32;
    let env = onset_envelope(audio, sample_rate, hop);
    // Up to 8 beats at the slowest tempo.
    let max_lag = ((8.0 * 60.0 / MIN_BPM * frame_rate) as usize).min(env.len().saturating_sub(2));
    if max_lag < 10 {
        return None;
    }
    // Loops wrap around; songs don't.
    let ac = autocorr(&env, max_lag, maybe_loop);
    if ac[0] <= 1e-12 {
        return None; // silence
    }

    let score = |bpm: f32| -> f32 {
        let period = 60.0 / bpm * frame_rate;
        let (mut sum, mut n) = (0.0, 0);
        for k in 1..=8 {
            let lag = period * k as f32;
            if lag >= max_lag as f32 {
                break;
            }
            sum += interp(&ac, lag);
            n += 1;
        }
        if n == 0 {
            return 0.0;
        }
        let rhythm = (sum / n as f32 / ac[0]).max(0.0);
        let octaves = (bpm / PRIOR_CENTER_BPM).log2();
        let prior = (-0.5 * (octaves / 0.6).powi(2)).exp();
        rhythm * prior
    };

    // Coarse search, then refine around the best tempo.
    let mut best = (MIN_BPM, f32::MIN);
    let mut bpm = MIN_BPM;
    while bpm <= MAX_BPM {
        let s = score(bpm);
        if s > best.1 {
            best = (bpm, s);
        }
        bpm += 0.1;
    }
    let mut free = best;
    let mut bpm = best.0 - 0.1;
    while bpm <= best.0 + 0.1 {
        let s = score(bpm);
        if s > free.1 {
            free = (bpm, s);
        }
        bpm += 0.005;
    }

    let bar_candidates = if maybe_loop {
        [1.0f32, 2.0, 4.0, 8.0, 16.0, 32.0]
            .iter()
            .map(|bars| 60.0 * bars * 4.0 / secs)
            .filter(|bpm| (MIN_BPM..=MAX_BPM).contains(bpm))
            .map(|bpm| (bpm, score(bpm)))
            .collect()
    } else {
        Vec::new()
    };
    Some(Analysis { secs, free, bar_candidates })
}

fn hop_size(sample_rate: u32) -> usize {
    crate::dsp::hop_size(sample_rate)
}

/// Spectral flux: per frame, the summed increase in log magnitude across
/// frequency bins, with the slow trend (~0.5 s moving average) removed.
pub(crate) fn onset_envelope(mono: &[f32], sample_rate: u32, hop: usize) -> Vec<f32> {
    let mut prev: Vec<f32> = Vec::new();
    let mut flux = Vec::new();
    crate::dsp::stft_log_mags(mono, sample_rate, hop, |_, mag| {
        if !prev.is_empty() {
            flux.push(mag.iter().zip(&prev).map(|(m, p)| (m - p).max(0.0)).sum());
        }
        prev.clear();
        prev.extend_from_slice(mag);
    });
    crate::dsp::detrend(&flux, 25)
}

/// Autocorrelation for lags 0..=max_lag, normalised per overlapping frame.
fn autocorr(x: &[f32], max_lag: usize, circular: bool) -> Vec<f32> {
    let n = x.len();
    (0..=max_lag)
        .map(|lag| {
            if circular {
                (0..n).map(|i| x[i] * x[(i + lag) % n]).sum::<f32>() / n as f32
            } else {
                let m = n - lag;
                (0..m).map(|i| x[i] * x[i + lag]).sum::<f32>() / m.max(1) as f32
            }
        })
        .collect()
}

fn interp(v: &[f32], pos: f32) -> f32 {
    let i = pos.floor() as usize;
    let f = pos - i as f32;
    match (v.get(i), v.get(i + 1)) {
        (Some(a), Some(b)) => a * (1.0 - f) + b * f,
        (Some(a), None) => *a,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::LoopAsset;

    #[test]
    fn explicit_names() {
        use NameBpm::Explicit;
        assert_eq!(bpm_from_name("Drums 124bpm"), Some(Explicit(124.0)));
        assert_eq!(bpm_from_name("bass_90_BPM_Am"), Some(Explicit(90.0)));
        assert_eq!(bpm_from_name("loop-128.5-bpm"), Some(Explicit(128.5)));
        assert_eq!(bpm_from_name("BPM140 hats"), Some(Explicit(140.0)));
        assert_eq!(bpm_from_name("Keys_Cmaj_bpm_100_v2"), Some(Explicit(100.0)));
        assert_eq!(
            bpm_from_name("J. Cole - Land of Snakes (Acapella) 104 [BPM]"),
            Some(Explicit(104.0))
        );
        assert_eq!(bpm_from_name("Soul Chop (88 BPM)"), Some(Explicit(88.0)));
        assert_eq!(bpm_from_name("[BPM] 93 rhodes"), Some(Explicit(93.0)));
    }

    #[test]
    fn bare_names() {
        use NameBpm::Bare;
        assert_eq!(bpm_from_name("cymatics_808_loop_140_cmin"), Some(Bare(140.0)));
        assert_eq!(bpm_from_name("OS_SL_90_drum_loop_2"), Some(Bare(90.0)));
        assert_eq!(bpm_from_name("Kick 01"), None);
        assert_eq!(bpm_from_name("Pack 2023 loop"), None);
        assert_eq!(bpm_from_name("hat120k"), None);
        assert_eq!(bpm_from_name("v1.25 pad"), None);
    }

    #[test]
    fn whole_bars() {
        assert!(is_whole_bars(4.0, 120.0)); // 2 bars
        assert!(is_whole_bars(6.76, 71.0)); // 2 bars, slightly trimmed
        assert!(!is_whole_bars(4.0, 100.0)); // 6.67 beats
        assert!(!is_whole_bars(0.3, 120.0)); // a one-shot
    }

    fn mono(stereo: &[f32]) -> Vec<f32> {
        stereo.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect()
    }

    #[test]
    fn detects_test_loop() {
        let a = LoopAsset::test_loop();
        assert_eq!(detect_bpm(&mono(&a.samples), a.sample_rate), Some(120.0));
    }

    #[test]
    fn detects_stretched_loops_exactly() {
        let a = LoopAsset::test_loop();
        let detect = |bpm: f32| {
            let r = a.stretch_to(bpm);
            detect_bpm(&mono(&r.samples), r.sample_rate).unwrap()
        };
        for bpm in [85.0, 95.0, 128.0, 140.0] {
            let got = detect(bpm);
            assert!((got - bpm).abs() < 0.02, "stretched to {bpm}, detected {got}");
        }
        // 170 vs 85 is genuinely ambiguous (DnB vs half-time): same audio either
        // way. Accept the octave; users fix it with the ½× / 2× buttons.
        let got = detect(170.0);
        assert!(close(got, 170.0, 0.002) || close(got, 85.0, 0.002), "170: detected {got}");
    }

    /// A "song": 100 s of the loop at 97.3 BPM, starting off the beat with
    /// a silent intro, and not a whole number of bars long.
    #[test]
    fn detects_song_tempo() {
        let r = LoopAsset::test_loop().stretch_to(97.3);
        let mut song = vec![0.0f32; (0.77 * 48_000.0) as usize];
        let one = mono(&r.samples);
        while song.len() < 100 * 48_000 {
            song.extend_from_slice(&one);
        }
        song.truncate(100 * 48_000 - 12_345);
        let got = detect_bpm(&song, 48_000).unwrap();
        assert!((got - 97.3).abs() <= 0.15, "detected {got}");
    }

    /// A chop that isn't whole bars (2.5 bars) shouldn't snap to a wrong
    /// bar-length tempo.
    #[test]
    fn detects_non_bar_chop() {
        let a = LoopAsset::test_loop();
        let m = mono(&a.samples);
        let chop: Vec<f32> = m.iter().chain(m.iter()).take(5 * 48_000).copied().collect();
        let got = detect_bpm(&chop, 48_000).unwrap();
        // 5 s of audio limits precision to roughly ±0.5%.
        assert!((got - 120.0).abs() <= 0.5, "detected {got}");
    }

    /// 8 bars of soft-attack chords at 127 BPM, one chord per bar: hardly
    /// any rhythm, so the length has to decide.
    #[test]
    fn detects_melodic_loop_by_length() {
        let sr = 48_000.0;
        let bar_secs = 4.0 * 60.0 / 127.0;
        let len = (8.0 * bar_secs * sr) as usize;
        let chords = [[220.0, 277.2, 329.6], [196.0, 246.9, 293.7], [174.6, 220.0, 261.6], [196.0, 246.9, 311.1]];
        let audio: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f32 / sr;
                let bar = (t / bar_secs) as usize;
                let in_bar = t - bar as f32 * bar_secs;
                let env = (in_bar / 0.25).min(1.0) * (1.0 - in_bar / bar_secs * 0.3);
                chords[bar % 4]
                    .iter()
                    .map(|f| (std::f32::consts::TAU * f * t).sin())
                    .sum::<f32>()
                    * 0.2
                    * env
            })
            .collect();
        let got = detect_bpm(&audio, 48_000).unwrap();
        assert!((got - 127.0).abs() < 0.05, "detected {got}");
    }

    #[test]
    fn one_shots_and_silence_have_no_tempo() {
        assert_eq!(detect_bpm(&vec![0.1; 24_000], 48_000), None);
        assert_eq!(detect_bpm(&vec![0.0; 10 * 48_000], 48_000), None);
    }
}
