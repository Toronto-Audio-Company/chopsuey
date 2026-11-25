//! Generates `assets/test_loop_120bpm.wav`: a 2-bar, 120 BPM drum + pluck loop.
//!
//! Run with: `cargo run --example make_test_loop`
//!
//! Every sound's tail wraps around to the start, so the file loops seamlessly.
//! The pluck line is there so you can hear that time-stretching keeps the pitch.

use std::f32::consts::TAU;

const SAMPLE_RATE: u32 = 48_000;
const BPM: f32 = 120.0;
const BARS: usize = 2;
const STEPS_PER_BAR: usize = 16; // 16th notes

fn main() {
    let step_len = (SAMPLE_RATE as f32 * 60.0 / BPM / 4.0) as usize;
    let total_steps = BARS * STEPS_PER_BAR;
    let len = step_len * total_steps;
    let mut left = vec![0.0f32; len];
    let mut right = vec![0.0f32; len];
    let mut rng = XorShift(0x9E37_79B9);

    // Adds a mono sound at `step`, panned (-1 = left, 1 = right), wrapping past the end.
    let mut add = |step: usize, pan: f32, sound: &[f32]| {
        let (gl, gr) = ((1.0 - pan) * 0.5, (1.0 + pan) * 0.5);
        let start = step * step_len;
        for (i, s) in sound.iter().enumerate() {
            let j = (start + i) % len;
            left[j] += s * gl * 2.0f32.sqrt();
            right[j] += s * gr * 2.0f32.sqrt();
        }
    };

    let kick = kick();
    let snare = snare(&mut rng);
    let hat_closed = hat(&mut rng, 0.045);
    let hat_open = hat(&mut rng, 0.25);

    for step in [0, 8, 10, 16, 24, 27] {
        add(step, 0.0, &kick);
    }
    for step in [4, 12, 20, 28] {
        add(step, 0.0, &snare);
    }
    for step in 0..total_steps {
        match step {
            30 => add(step, 0.3, &hat_open),
            s if s % 2 == 0 => add(step, 0.3, &hat_closed),
            _ => add(step, 0.3, &scaled(&hat_closed, 0.35)),
        }
    }
    // (step, midi note, length in steps)
    let melody = [
        (0, 57, 3), (3, 57, 2), (6, 60, 2), (8, 64, 4), (14, 62, 2),
        (16, 57, 3), (19, 57, 2), (22, 67, 2), (24, 64, 4), (30, 62, 2),
    ];
    for (step, note, steps) in melody {
        add(step, -0.25, &pluck(note, steps * step_len));
    }

    // Normalize to -1 dBFS.
    let peak = left.iter().chain(&right).fold(0.0f32, |m, s| m.max(s.abs()));
    let norm = 10f32.powf(-1.0 / 20.0) / peak;

    std::fs::create_dir_all("assets").unwrap();
    let path = "assets/test_loop_120bpm.wav";
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    let max = (1 << 23) as f32 - 1.0;
    for (l, r) in left.iter().zip(&right) {
        w.write_sample((l * norm * max) as i32).unwrap();
        w.write_sample((r * norm * max) as i32).unwrap();
    }
    w.finalize().unwrap();
    println!(
        "wrote {path}: {BARS} bars @ {BPM} BPM, {:.2}s, {SAMPLE_RATE} Hz",
        len as f32 / SAMPLE_RATE as f32
    );
}

fn secs(s: f32) -> usize {
    (s * SAMPLE_RATE as f32) as usize
}

fn kick() -> Vec<f32> {
    let mut phase = 0.0f32;
    (0..secs(0.45))
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            let freq = 48.0 + 110.0 * (-t * 35.0).exp();
            phase += TAU * freq / SAMPLE_RATE as f32;
            phase.sin() * (-t * 7.0).exp() * 0.9
        })
        .collect()
}

fn snare(rng: &mut XorShift) -> Vec<f32> {
    let mut prev = 0.0;
    (0..secs(0.25))
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            let n = rng.next();
            let bright = n - prev; // crude high-pass
            prev = n;
            let tone = (TAU * 185.0 * t).sin() * (-t * 30.0).exp();
            (bright * 0.5 * (-t * 18.0).exp() + tone * 0.5) * 0.7
        })
        .collect()
}

fn hat(rng: &mut XorShift, decay_secs: f32) -> Vec<f32> {
    let mut prev = 0.0;
    (0..secs(decay_secs * 4.0))
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            let n = rng.next();
            let hp = n - prev;
            prev = n;
            hp * (-t / decay_secs).exp() * 0.18
        })
        .collect()
}

fn pluck(midi: i32, len: usize) -> Vec<f32> {
    let freq = 440.0 * 2f32.powf((midi - 69) as f32 / 12.0);
    (0..len)
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            // A few harmonics, higher ones decaying faster: a soft saw-like pluck.
            let s: f32 = (1..=5)
                .map(|h| {
                    let h = h as f32;
                    (TAU * freq * h * t).sin() / h * (-t * (4.0 + h * 3.0)).exp()
                })
                .sum();
            let release = ((len - i) as f32 / secs(0.01) as f32).min(1.0);
            s * 0.35 * release
        })
        .collect()
}

fn scaled(s: &[f32], g: f32) -> Vec<f32> {
    s.iter().map(|x| x * g).collect()
}

struct XorShift(u32);
impl XorShift {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as f32 / u32::MAX as f32 * 2.0 - 1.0
    }
}
