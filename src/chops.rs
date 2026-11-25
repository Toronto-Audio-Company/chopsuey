//! Finding loopable, bar-aligned chops in a song.
//!
//! 1. Spectral analysis every 10 ms: onset strength (all bins and the low
//!    end), plus a 16-band spectral "colour" per frame.
//! 2. Beat tracking by dynamic programming (Ellis 2007): beats should land
//!    on onsets and be about one beat period apart, but may drift, which
//!    matters for records played by a live drummer.
//! 3. Downbeats: of the four possible "beat 1" positions, the one with the
//!    strongest low-end (kick) onsets.
//! 4. Candidates: every 1, 2 and 4-bar span starting on a downbeat. A span
//!    loops well if the span next to it sounds the same (it repeats in the
//!    song) and the bar after it sounds like its first bar (the loop point
//!    is seamless).
//! 5. For each length, greedily keep the best spans that don't overlap and
//!    don't sound alike, then fill up to at least 10.

use crate::dsp;

pub const CHOP_LENGTHS_BARS: [usize; 3] = [1, 2, 4];
/// Every song offers at least this many chops of each length (if it has
/// enough bars)...
pub const MIN_CHOPS: usize = 10;
/// ...and at most this many.
pub const MAX_CHOPS: usize = 15;
const BANDS: usize = 16;
/// How strongly beats are held to the tempo (librosa's default).
const TIGHTNESS: f32 = 100.0;
/// Chops start this long before the detected beat, so attacks aren't clipped.
const PRE_ROLL_SECS: f32 = 0.004;
/// A chop with this base score (mean of repeat and seam similarity) or more
/// is a clean loop.
const CLEAN_LOOP: f32 = 0.8;
/// If there are fewer than `MIN_CHOPS` clean loops, decent ones down to this
/// score fill in first, then anything bar-aligned.
const FALLBACK_LOOP: f32 = 0.6;
/// A chop whose length is further than this from the song tempo is assumed
/// to have a misplaced beat (common without drums) and is cut to exactly
/// N bars at the song tempo instead. Real drift over a few bars is smaller.
const MAX_LOCAL_DRIFT: f32 = 0.015;
/// Two chops whose average spectrum is this similar count as the same groove.
const SAME_GROOVE: f32 = 0.92;

#[derive(Clone, Debug, PartialEq)]
pub struct Chop {
    /// Sample frames in the song (start inclusive, end exclusive).
    pub start: usize,
    pub end: usize,
    /// Bar number in the song (0-based) and length in bars.
    pub first_bar: usize,
    pub bars: usize,
    /// Exact tempo of this span, so it stretches to exactly `bars` bars.
    pub bpm: f32,
    pub score: f32,
}

#[derive(Clone, Debug, Default)]
pub struct SongAnalysis {
    /// Beat positions in sample frames.
    pub beats: Vec<usize>,
    /// Bar start positions in sample frames.
    pub bar_starts: Vec<usize>,
    /// Suggested chops, grouped by length (1, 2, 4 bars), each in song order.
    pub chops: Vec<Chop>,
}

/// Analyses mono audio whose overall tempo is about `bpm`.
pub fn analyse_song(mono: &[f32], sample_rate: u32, bpm: f32) -> SongAnalysis {
    let hop = dsp::hop_size(sample_rate);
    let n_fft = dsp::fft_size(sample_rate);
    let frame_rate = sample_rate as f32 / hop as f32;
    let low_bins = ((150.0 * n_fft as f32 / sample_rate as f32) as usize).max(2);
    let band_of_bin = band_map(n_fft, sample_rate);

    // 1. Features.
    let mut flux = Vec::new();
    let mut low_flux = Vec::new();
    let mut colour: Vec<[f32; BANDS]> = Vec::new();
    let mut prev: Vec<f32> = Vec::new();
    dsp::stft_log_mags(mono, sample_rate, hop, |_, mag| {
        if prev.is_empty() {
            flux.push(0.0);
            low_flux.push(0.0);
        } else {
            let rise = |range: std::ops::Range<usize>| -> f32 {
                range.map(|k| (mag[k] - prev[k]).max(0.0)).sum()
            };
            flux.push(rise(0..mag.len()));
            low_flux.push(rise(1..low_bins));
        }
        let mut c = [0.0f32; BANDS];
        let mut counts = [0usize; BANDS];
        for (k, &m) in mag.iter().enumerate() {
            if let Some(b) = band_of_bin[k] {
                c[b] += m;
                counts[b] += 1;
            }
        }
        for b in 0..BANDS {
            c[b] /= counts[b].max(1) as f32;
        }
        colour.push(c);
        prev.clear();
        prev.extend_from_slice(mag);
    });
    if flux.len() < 50 {
        return SongAnalysis::default();
    }

    // 2. Beats.
    let period = 60.0 / bpm * frame_rate;
    let env = normalise(&dsp::detrend(&flux, 25));
    let beat_frames = track_beats(&env, period);
    if beat_frames.len() < 8 {
        return SongAnalysis::default();
    }
    // Frame f's window is centred n_fft/2 samples after its start; onsets
    // register about a quarter window early.
    let frame_to_sample = |f: usize| -> usize {
        let pre = (PRE_ROLL_SECS * sample_rate as f32) as usize;
        (f * hop + n_fft / 4).saturating_sub(pre).min(mono.len())
    };

    // 3. Downbeat phase.
    let low = normalise(&dsp::detrend(&low_flux, 25));
    let strength = |f: usize| -> f32 {
        (f.saturating_sub(2)..(f + 3).min(low.len())).map(|i| low[i]).fold(f32::MIN, f32::max)
    };
    let phase = (0..4)
        .max_by(|&a, &b| {
            let mean = |k: usize| -> f32 {
                let v: Vec<f32> = beat_frames.iter().skip(k).step_by(4).map(|&f| strength(f)).collect();
                v.iter().sum::<f32>() / v.len().max(1) as f32
            };
            mean(a).total_cmp(&mean(b))
        })
        .unwrap_or(0);
    let bar_beats: Vec<usize> = (phase..beat_frames.len()).step_by(4).collect();

    // Per-beat spectral colour, standardised per band (song mean 0, spread
    // 1) so a change in any band counts, not just in the busiest ones.
    let mean_colour = average(&colour);
    let spread = {
        let mut v = [0.0f32; BANDS];
        for c in &colour {
            for b in 0..BANDS {
                v[b] += (c[b] - mean_colour[b]).powi(2);
            }
        }
        v.map(|x| (x / colour.len() as f32).sqrt().max(1e-6))
    };
    let beat_colour: Vec<[f32; BANDS]> = beat_frames
        .windows(2)
        .map(|w| {
            let mut c = [0.0f32; BANDS];
            for f in w[0]..w[1].min(colour.len()) {
                for b in 0..BANDS {
                    c[b] += (colour[f][b] - mean_colour[b]) / spread[b];
                }
            }
            c.map(|x| x / (w[1] - w[0]).max(1) as f32)
        })
        .collect();
    let seg = |first_beat: usize, beats: usize| -> Option<Vec<f32>> {
        let slice = beat_colour.get(first_beat..first_beat + beats)?;
        Some(slice.iter().flat_map(|c| c.iter().copied()).collect())
    };

    // 4. Candidates.
    let bar_rms: Vec<f32> = bar_beats
        .windows(2)
        .map(|w| rms(&mono[frame_to_sample(beat_frames[w[0]])..frame_to_sample(beat_frames[w[1]])]))
        .collect();
    let loudest = bar_rms.iter().copied().fold(0.0f32, f32::max);
    let mut candidates = Vec::new();
    for &bars in &CHOP_LENGTHS_BARS {
        let beats = bars * 4;
        for bar in 0..bar_beats.len() {
            let b0 = bar_beats[bar];
            let Some(&end_beat) = beat_frames.get(b0 + beats) else { continue };
            let Some(this) = seg(b0, beats) else { continue };
            // Skip near-silent spans (intros, fades).
            if (bar..bar + bars).any(|i| bar_rms.get(i).is_none_or(|&r| r < loudest * 0.1)) {
                continue;
            }
            // Seam: the bar after the chop should sound like its first bar,
            // so jumping back to the start is musically continuous. This
            // rules out spans that cross into a different section.
            let (Some(first), Some(after)) = (seg(b0, 4), seg(b0 + beats, 4)) else { continue };
            let seam = cosine(&first, &after);
            // Repeat: the neighbouring span is the same music.
            let next = seg(b0 + beats, beats).map(|n| cosine(&this, &n));
            let prev = b0.checked_sub(beats).and_then(|p| seg(p, beats)).map(|p| cosine(&this, &p));
            let Some(repeat) = next.into_iter().chain(prev).reduce(f32::max) else { continue };
            let start = frame_to_sample(beat_frames[b0]);
            let mut end = frame_to_sample(end_beat);
            let expected = (beats as f32 * 60.0 / bpm * sample_rate as f32) as usize;
            if ((end - start) as f32 / expected as f32 - 1.0).abs() > MAX_LOCAL_DRIFT {
                end = (start + expected).min(mono.len());
            }
            let secs = (end - start) as f32 / sample_rate as f32;
            candidates.push((
                Chop {
                    start,
                    end,
                    first_bar: bar,
                    bars,
                    bpm: 60.0 * beats as f32 / secs,
                    // Slight preference for longer loops when equally loopable.
                    score: 0.5 * (repeat + seam),
                },
                average(&beat_colour[b0..b0 + beats]),
            ));
        }
    }

    // 5. Pick each length separately, so every length offers MIN chops.
    candidates.sort_by(|a, b| b.0.score.total_cmp(&a.0.score));
    let mut chops: Vec<Chop> = CHOP_LENGTHS_BARS
        .iter()
        .flat_map(|&bars| {
            let of_length: Vec<_> = candidates.iter().filter(|(c, _)| c.bars == bars).cloned().collect();
            pick(&of_length)
        })
        .collect();
    chops.sort_by_key(|c| (c.bars, c.start));

    SongAnalysis {
        beats: beat_frames.iter().map(|&f| frame_to_sample(f)).collect(),
        bar_starts: bar_beats.iter().map(|&b| frame_to_sample(beat_frames[b])).collect(),
        chops,
    }
}

/// Picks chops of one length from candidates sorted best first, in passes:
///   a. clean loops that don't sound like one already picked (up to MAX),
///   b. decent loops that don't sound alike (up to MIN),
///   c. anything (up to MIN), never overlapping,
///   d. still short (short songs)? Allow overlap, just never two chops
///      starting on the same bar.
fn pick(candidates: &[(Chop, [f32; BANDS])]) -> Vec<Chop> {
    let mut picked: Vec<(Chop, [f32; BANDS])> = Vec::new();
    let passes: [(f32, bool, bool, usize); 4] = [
        (CLEAN_LOOP, true, false, MAX_CHOPS),
        (FALLBACK_LOOP, true, false, MIN_CHOPS),
        (f32::MIN, false, false, MIN_CHOPS),
        (f32::MIN, false, true, MIN_CHOPS),
    ];
    for (min_score, distinct, allow_overlap, limit) in passes {
        for (chop, groove) in candidates {
            if picked.len() >= limit {
                break;
            }
            if chop.score < min_score {
                continue;
            }
            let overlaps = if allow_overlap {
                picked.iter().any(|(p, _)| p.first_bar == chop.first_bar)
            } else {
                picked.iter().any(|(p, _)| chop.start < p.end && p.start < chop.end)
            };
            let same = distinct && picked.iter().any(|(_, g)| cosine(groove, g) > SAME_GROOVE);
            if !overlaps && !same {
                picked.push((chop.clone(), *groove));
            }
        }
    }
    picked.into_iter().map(|(c, _)| c).collect()
}

/// Dynamic-programming beat tracker. `env` is the onset strength per frame,
/// `period` the expected beat spacing in frames. Returns beat frames.
fn track_beats(env: &[f32], period: f32) -> Vec<usize> {
    let n = env.len();
    // Lightly smoothed onset strength.
    let sigma = (period / 32.0).max(0.5);
    let radius = (sigma * 3.0).ceil() as isize;
    let kernel: Vec<f32> = (-radius..=radius).map(|i| (-0.5 * (i as f32 / sigma).powi(2)).exp()).collect();
    let local: Vec<f32> = (0..n as isize)
        .map(|i| {
            (-radius..=radius)
                .filter_map(|k| env.get((i + k) as usize).map(|v| v * kernel[(k + radius) as usize]))
                .sum()
        })
        .collect();

    let lo = (period / 2.0).round().max(1.0) as usize;
    let hi = (period * 2.0).round() as usize;
    let mut cum = vec![0.0f32; n];
    let mut back = vec![usize::MAX; n];
    // The penalty depends only on the gap t - tau: work it out once per gap
    // (same expression, so the same values) instead of per pair.
    let cost: Vec<f32> = (0..=hi)
        .map(|d| {
            let penalty = (d as f32 / period).ln();
            TIGHTNESS * penalty * penalty
        })
        .collect();
    for t in 0..n {
        let mut best = (f32::MIN, usize::MAX);
        if t >= lo {
            for tau in t.saturating_sub(hi)..=t - lo {
                let v = cum[tau] - cost[t - tau];
                if v > best.0 {
                    best = (v, tau);
                }
            }
        }
        if best.1 != usize::MAX && best.0 > 0.0 {
            cum[t] = local[t] + best.0;
            back[t] = best.1;
        } else {
            cum[t] = local[t];
        }
    }
    // Last beat: the best score in the final two periods.
    let tail = n.saturating_sub((period * 2.0) as usize);
    let mut t = (tail..n).max_by(|&a, &b| cum[a].total_cmp(&cum[b])).unwrap_or(0);
    let mut beats = vec![t];
    while back[t] != usize::MAX {
        t = back[t];
        beats.push(t);
    }
    beats.reverse();
    beats
}

/// Maps FFT bins to 16 log-spaced bands from 60 Hz to 12 kHz.
fn band_map(n_fft: usize, sample_rate: u32) -> Vec<Option<usize>> {
    let (lo, hi) = (60.0f32, 12_000.0f32);
    (0..=n_fft / 2)
        .map(|k| {
            let hz = k as f32 * sample_rate as f32 / n_fft as f32;
            if hz < lo || hz >= hi {
                return None;
            }
            Some((((hz / lo).ln() / (hi / lo).ln()) * BANDS as f32) as usize)
        })
        .map(|b| b.map(|b| b.min(BANDS - 1)))
        .collect()
}

fn normalise(x: &[f32]) -> Vec<f32> {
    let sd = (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt().max(1e-9);
    x.iter().map(|v| v / sd).collect()
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb).max(1e-9)
}

fn average(beats: &[[f32; BANDS]]) -> [f32; BANDS] {
    let mut m = [0.0f32; BANDS];
    for c in beats {
        for b in 0..BANDS {
            m[b] += c[b];
        }
    }
    m.map(|x| x / beats.len().max(1) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::LoopAsset;

    const SR: u32 = 48_000;
    const BPM: f32 = 97.0;
    const INTRO: f32 = 0.5;

    fn mono(stereo: &[f32]) -> Vec<f32> {
        stereo.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect()
    }

    /// Intro silence, then section A (the drum loop) x4, B (the same loop
    /// high-passed: no kick or bass) x2, then A x2. Each loop is 2 bars.
    fn song() -> (Vec<f32>, f32) {
        let a = mono(&LoopAsset::test_loop().stretch_to(BPM).samples);
        let coef = (-std::f32::consts::TAU * 400.0 / SR as f32).exp();
        let mut lp = 0.0;
        let b: Vec<f32> = a
            .iter()
            .map(|&x| {
                lp = coef * lp + (1.0 - coef) * x;
                (x - lp) * 1.5
            })
            .collect();
        let mut s = vec![0.0f32; (INTRO * SR as f32) as usize];
        for part in [&a, &a, &a, &a, &b, &b, &a, &a] {
            s.extend_from_slice(part);
        }
        let bar_secs = 4.0 * 60.0 / BPM;
        (s, bar_secs)
    }

    #[test]
    fn finds_beats_and_bar_aligned_chops() {
        let (audio, bar_secs) = song();
        let a = analyse_song(&audio, SR, BPM);
        let beat_secs = 60.0 / BPM;

        // Beats land on the grid.
        let errs: Vec<f32> = a
            .beats
            .iter()
            .map(|&b| {
                let t = b as f32 / SR as f32 - INTRO;
                let nearest = (t / beat_secs).round() * beat_secs;
                (t - nearest).abs()
            })
            .collect();
        let mut sorted = errs.clone();
        sorted.sort_by(f32::total_cmp);
        let median = sorted[sorted.len() / 2];
        eprintln!("beats: {}, median error {:.4}s, max {:.4}s", a.beats.len(), median, sorted.last().unwrap());
        for c in &a.chops {
            eprintln!("chop bar {} x{} at {:.3}s bpm {:.2} score {:.3}", c.first_bar, c.bars, c.start as f32 / SR as f32, c.bpm, c.score);
        }
        assert!(median < 0.015, "median beat error {median}s");

        // At least MIN_CHOPS of every length (the song has 16 bars).
        for bars in CHOP_LENGTHS_BARS {
            let n = a.chops.iter().filter(|c| c.bars == bars).count();
            assert!(n >= MIN_CHOPS, "only {n} chops of {bars} bars");
        }
        // Chops start on downbeats (where the loop's kick on 1 is).
        for c in &a.chops {
            let t = c.start as f32 / SR as f32 - INTRO;
            let off = (t - (t / bar_secs).round() * bar_secs).abs();
            assert!(off < 0.025, "chop at {t}s is {off}s off a downbeat: {c:?}");
            assert!((c.bpm - BPM).abs() / BPM < 0.01, "chop tempo {} {c:?}", c.bpm);
            assert!(c.end > c.start);
        }

        // Both the drum section and the filtered section are represented.
        let section_b = (INTRO + 8.0 * bar_secs, INTRO + 12.0 * bar_secs);
        let in_b = |c: &Chop| {
            let t = c.start as f32 / SR as f32;
            t >= section_b.0 - 0.05 && t < section_b.1
        };
        assert!(a.chops.iter().any(in_b), "no chop from section B: {:?}", a.chops);
        assert!(a.chops.iter().any(|c| !in_b(c)), "no chop from section A");

        // Never two chops of the same length starting on the same bar.
        for w in a.chops.windows(2) {
            assert!(w[0].bars != w[1].bars || w[0].start != w[1].start);
        }
    }

    #[test]
    fn silence_gives_no_chops() {
        let a = analyse_song(&vec![0.0; 30 * SR as usize], SR, 100.0);
        assert!(a.chops.is_empty());
    }
}
