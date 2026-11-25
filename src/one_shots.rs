//! Finding one-shots in a song: single sounds or phrases with quiet on
//! either side (a trumpet blast, a shout, a spoken line), as opposed to
//! the bar-aligned loops in `chops`.
//!
//! 1. Loudness every 10 ms (RMS over 20 ms, in dB).
//! 2. Thresholds from the song's own range: a sound starts above `open`
//!    (30 dB under the loud parts, or halfway up from the quiet parts if
//!    the song never gets that quiet) and ends once it has stayed under
//!    `close` (6 dB lower) for a pause of `GAP_SECS`. Shorter pauses, like
//!    those between words, stay inside the phrase.
//! 3. Sounds that never get near the loud parts (breaths, room noise) or
//!    are too short (clicks) are dropped; long ones are cut at their
//!    quietest moments into pieces of at most `MAX_SECS`.
//! 4. At most `MAX_ONE_SHOTS`, the loudest, in song order.

use crate::chops::Chop;

/// `Chop::bars` of a one-shot (it isn't bar-aligned).
pub const ONE_SHOT: usize = 0;
/// Pauses shorter than this stay inside a sound (gaps between words).
const GAP_SECS: f32 = 0.25;
const MIN_SECS: f32 = 0.15;
const MAX_SECS: f32 = 6.0;
/// A long sound is only cut where both pieces are at least this long.
const MIN_PIECE_SECS: f32 = 1.0;
pub const MAX_ONE_SHOTS: usize = 40;
/// Sounds whose peak is this far under the song's loud parts are dropped.
const MAX_BELOW_LOUD_DB: f32 = 18.0;

/// One-shots in mono audio, in song order. `bpm` is only recorded (a
/// one-shot plays at its own speed).
pub fn find_one_shots(mono: &[f32], sample_rate: u32, bpm: f32) -> Vec<Chop> {
    let hop = (sample_rate as usize / 100).max(1);
    let frames = mono.len() / hop;
    if frames < 4 {
        return Vec::new();
    }
    let secs_to_frames = |s: f32| (s * 100.0).round() as usize;
    // 1. Loudness per 10 ms frame (20 ms window).
    let env: Vec<f32> = (0..frames)
        .map(|i| {
            let w = &mono[i * hop..((i + 2) * hop).min(mono.len())];
            let ms = w.iter().map(|x| x * x).sum::<f32>() / w.len() as f32;
            10.0 * ms.max(1e-10).log10()
        })
        .collect();

    // 2. Thresholds.
    let mut sorted = env.clone();
    sorted.sort_by(f32::total_cmp);
    let pct = |p: f32| sorted[((sorted.len() - 1) as f32 * p) as usize];
    let (loud, quiet) = (pct(0.95), pct(0.10));
    let open = (loud - 30.0).max((loud + quiet) / 2.0);
    let close = open - 6.0;
    let gap = secs_to_frames(GAP_SECS);

    let mut sounds: Vec<(usize, usize)> = Vec::new(); // frame ranges
    let mut i = 0;
    while i < frames {
        if env[i] <= open {
            i += 1;
            continue;
        }
        // Back up over the attack's quiet start (up to 50 ms).
        let floor = sounds.last().map_or(0, |s| s.1);
        let mut start = i;
        while start > floor && i - start < 5 && env[start - 1] > close {
            start -= 1;
        }
        // Until a long enough pause.
        let mut last_loud = i;
        let mut j = i + 1;
        while j < frames && j - last_loud <= gap {
            if env[j] > close {
                last_loud = j;
            }
            j += 1;
        }
        // 30 ms of release after the last loud frame, 10 ms before the start.
        sounds.push((start.saturating_sub(1).max(floor), (last_loud + 3).min(frames)));
        i = j;
    }

    // 3. Too quiet or too short out; too long cut up.
    let peak = |(a, b): (usize, usize)| env[a..b].iter().copied().fold(f32::MIN, f32::max);
    let mut pieces = Vec::new();
    for s in sounds {
        if peak(s) < loud - MAX_BELOW_LOUD_DB {
            continue;
        }
        split(&env, s, secs_to_frames(MAX_SECS), secs_to_frames(MIN_PIECE_SECS), &mut pieces);
    }
    pieces.retain(|&(a, b)| b - a >= secs_to_frames(MIN_SECS));

    // 4. The loudest, in order.
    if pieces.len() > MAX_ONE_SHOTS {
        let mut by_peak: Vec<(usize, usize)> = pieces.clone();
        by_peak.sort_by(|&a, &b| peak(b).total_cmp(&peak(a)));
        by_peak.truncate(MAX_ONE_SHOTS);
        pieces.retain(|p| by_peak.contains(p));
    }
    let len = mono.len();
    pieces
        .into_iter()
        .enumerate()
        .map(|(n, (a, b))| Chop {
            start: (a * hop).min(len),
            end: (b * hop).min(len),
            first_bar: n,
            bars: ONE_SHOT,
            bpm,
            score: peak((a, b)),
        })
        .collect()
}

/// Adds `s` to `out`, cut at its quietest points (50 ms average) into
/// pieces no longer than `max` where that leaves pieces of `min_piece`.
fn split(env: &[f32], s: (usize, usize), max: usize, min_piece: usize, out: &mut Vec<(usize, usize)>) {
    let (a, b) = s;
    if b - a <= max || b - a < 2 * min_piece {
        out.push(s);
        return;
    }
    let smooth = |i: usize| {
        let w = &env[i.saturating_sub(2)..(i + 3).min(env.len())];
        w.iter().sum::<f32>() / w.len() as f32
    };
    let cut = (a + min_piece..b - min_piece).min_by(|&x, &y| smooth(x).total_cmp(&smooth(y))).unwrap();
    split(env, (a, cut), max, min_piece, out);
    split(env, (cut, b), max, min_piece, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    /// `(start, length, level)` bursts of noise-ish tone, in seconds.
    fn bursts(total: f32, parts: &[(f32, f32, f32)]) -> Vec<f32> {
        let mut x = vec![0.0f32; (total * SR as f32) as usize];
        for &(start, len, level) in parts {
            let a = (start * SR as f32) as usize;
            for i in a..a + (len * SR as f32) as usize {
                let t = i as f32 / SR as f32;
                x[i] = level * ((std::f32::consts::TAU * 220.0 * t).sin() + 0.3 * (std::f32::consts::TAU * 1337.0 * t).sin());
            }
        }
        x
    }

    #[test]
    fn finds_sounds_between_pauses() {
        let x = bursts(10.0, &[(1.0, 0.5, 0.5), (3.0, 1.2, 0.4), (6.0, 0.8, 0.5)]);
        let shots = find_one_shots(&x, SR, 120.0);
        assert_eq!(shots.len(), 3, "{shots:?}");
        for (shot, (start, len)) in shots.iter().zip([(1.0, 0.5), (3.0, 1.2), (6.0, 0.8)]) {
            let (s, e) = (shot.start as f32 / SR as f32, shot.end as f32 / SR as f32);
            assert!((s - start).abs() < 0.03, "starts at {s}, want {start}");
            assert!(e >= start + len && e < start + len + 0.06, "ends at {e}, want {}", start + len);
            assert_eq!(shot.bars, ONE_SHOT);
        }
    }

    #[test]
    fn short_pauses_stay_inside_a_phrase() {
        // Three "words" 100 ms apart: one phrase.
        let x = bursts(6.0, &[(1.0, 0.4, 0.5), (1.5, 0.4, 0.5), (2.0, 0.4, 0.5)]);
        let shots = find_one_shots(&x, SR, 120.0);
        assert_eq!(shots.len(), 1, "{shots:?}");
    }

    #[test]
    fn quiet_and_tiny_sounds_are_dropped() {
        // A breath 30 dB down and a 50 ms click.
        let x = bursts(8.0, &[(1.0, 1.0, 0.5), (3.0, 0.5, 0.015), (5.0, 0.05, 0.5)]);
        let shots = find_one_shots(&x, SR, 120.0);
        assert_eq!(shots.len(), 1, "{shots:?}");
    }

    #[test]
    fn long_sounds_are_cut_at_dips() {
        // 14 s of sound with a dip (quieter, not silent) at 5 s and 10 s.
        let mut x = bursts(16.0, &[(1.0, 14.0, 0.5)]);
        for dip in [5.0f32, 10.0] {
            let a = (dip * SR as f32) as usize;
            x[a..a + SR as usize / 10].iter_mut().for_each(|s| *s *= 0.05);
        }
        let shots = find_one_shots(&x, SR, 120.0);
        assert_eq!(shots.len(), 3, "{shots:?}");
        for s in &shots {
            assert!((s.end - s.start) as f32 / SR as f32 <= MAX_SECS + 0.01);
        }
        let cut = shots[0].end as f32 / SR as f32;
        assert!((cut - 5.05).abs() < 0.1, "cut at {cut}");
    }

    #[test]
    fn silence_has_none() {
        assert!(find_one_shots(&vec![0.0; SR as usize * 5], SR, 120.0).is_empty());
    }
}
