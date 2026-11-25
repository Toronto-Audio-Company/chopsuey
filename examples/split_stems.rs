//! Splits a WAV into 4 stems with the bundled HTDemucs model and reports
//! timing, how the energy divides between stems, and how well the stems sum
//! back to the original.
//!
//! cargo run --release --example split_stems -- <model.onnx> <song.wav> <out-dir> [gpu|cpu|gpu+cpuN|cpuN]

use std::path::PathBuf;
use std::time::Instant;
use tempo_sampler::stems::{Engine, Separator, STEM_NAMES};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let model = PathBuf::from(&args[1]);
    let song = PathBuf::from(&args[2]);
    let out = PathBuf::from(&args[3]);
    let spec = args.get(4).cloned().unwrap_or_else(|| "gpu+cpu4".into());
    let gpu = spec.contains("gpu");
    let cpu_threads = spec
        .split("cpu")
        .nth(1)
        .map(|n| n.parse().unwrap_or(Separator::default_threads()))
        .unwrap_or(0);
    let engine = Engine { gpu, cpu_threads };
    std::fs::create_dir_all(&out).unwrap();

    let (stereo, sr) = tempo_sampler::audio_file::decode(&song).unwrap();
    let secs = stereo.len() as f32 / 2.0 / sr as f32;
    println!("{}: {secs:.1} s at {sr} Hz, engine {engine:?}", song.display());

    let t = Instant::now();
    let mut sep = Separator::load(&model, engine).expect("load model");
    println!("devices: {}", sep.devices());
    println!("model loaded in {:.1} s", t.elapsed().as_secs_f32());

    let t = Instant::now();
    let mut last = 0;
    let stems = sep
        .separate(
            &stereo,
            sr,
            |p| {
                let pct = (p * 100.0) as i32;
                if pct / 25 > last {
                    last = pct / 25;
                    println!("  {pct}%");
                }
            },
            || false,
        )
        .expect("separate")
        .unwrap();
    let el = t.elapsed().as_secs_f32();
    println!("separated in {el:.1} s ({:.2}x realtime)", el / secs);

    let energy = |x: &[f32]| x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>();
    let total: f64 = stems.stems.iter().map(|s| energy(s)).sum();
    for (name, s) in STEM_NAMES.iter().zip(&stems.stems) {
        println!("  {name:7} {:5.1}% of stem energy", 100.0 * energy(s) / total);
        let spec = hound::WavSpec { channels: 2, sample_rate: sr, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(out.join(format!("{name}.wav")), spec).unwrap();
        s.iter().for_each(|v| w.write_sample(*v).unwrap());
        w.finalize().unwrap();
    }
    let residual: f64 = (0..stereo.len())
        .map(|i| {
            let sum: f32 = stems.stems.iter().map(|s| s[i]).sum();
            ((stereo[i] - sum) as f64).powi(2)
        })
        .sum();
    println!(
        "sum of stems vs original: {:.1} dB residual",
        10.0 * (residual / energy(&stereo).max(1e-12)).log10()
    );
}
