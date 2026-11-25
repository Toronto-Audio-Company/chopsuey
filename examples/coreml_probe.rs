//! Runs one 7.8 s window of a song through HTDemucs on CPU (reference) and
//! with one CoreML configuration, and reports how close CoreML's stems are
//! to the CPU ones (SNR per stem, dB; higher is better, > 40 dB is
//! essentially identical) and the time per window.
//!
//! cargo run --release --example coreml_probe -- <model.onnx> <song.wav> <config>
//! config: cpu | webgpu | <units>-<format>[-static][-subgraphs][-lowprec]
//!   units: all | gpu | ane     format: mlprogram | nn

use std::path::PathBuf;
use std::time::Instant;

use ort::ep::coreml::{ComputeUnits, ModelFormat};
use ort::session::Session;
use ort::value::Tensor;
use tempo_sampler::dsp::resample_stereo;
use tempo_sampler::loops::decode_wav;
use tempo_sampler::stems::SEGMENT;

fn session(model: &PathBuf, config: &str) -> Session {
    let mut b = Session::builder().unwrap().with_intra_threads(6).unwrap();
    if config.starts_with("webgpu") {
        let mut ep = ort::ep::WebGPU::default();
        if config.contains("capture") {
            ep = ep.with_enable_graph_capture(true);
        }
        if config.contains("nhwc") {
            ep = ep.with_preferred_layout(ort::ep::webgpu::PreferredLayout::NHWC);
        }
        if config.contains("nchw") {
            ep = ep.with_preferred_layout(ort::ep::webgpu::PreferredLayout::NCHW);
        }
        b = b.with_execution_providers([ep.build().error_on_failure()]).unwrap();
    } else if config != "cpu" {
        let parts: Vec<&str> = config.split('-').collect();
        let units = match parts[0] {
            "gpu" => ComputeUnits::CPUAndGPU,
            "ane" => ComputeUnits::CPUAndNeuralEngine,
            _ => ComputeUnits::All,
        };
        let format = if parts[1] == "nn" { ModelFormat::NeuralNetwork } else { ModelFormat::MLProgram };
        let cache = std::env::temp_dir().join(format!("ts-coreml-{config}"));
        std::fs::create_dir_all(&cache).unwrap();
        let ep = ort::ep::CoreML::default()
            .with_compute_units(units)
            .with_model_format(format)
            .with_static_input_shapes(parts.contains(&"static"))
            .with_subgraphs(parts.contains(&"subgraphs"))
            .with_low_precision_accumulation_on_gpu(parts.contains(&"lowprec"))
            .with_model_cache_dir(cache.display());
        b = b.with_execution_providers([ep.build().error_on_failure()]).unwrap();
    }
    b.commit_from_file(model).unwrap()
}

fn run(s: &mut Session, chunk: &[f32]) -> Vec<f32> {
    let input = Tensor::from_array(([1usize, 2, SEGMENT], chunk.to_vec())).unwrap();
    let out = s.run(ort::inputs![input]).unwrap();
    out[0].try_extract_tensor::<f32>().unwrap().1.to_vec()
}

fn main() {
    if std::env::var_os("PROBE_LOG").is_some() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("ort=trace"))
            .with_writer(std::io::stderr)
            .init();
    }
    let a: Vec<String> = std::env::args().collect();
    let (model, song, config) = (PathBuf::from(&a[1]), PathBuf::from(&a[2]), a[3].clone());

    // A window from the middle of the song, at 44.1 kHz, planar, normalised.
    let (stereo, sr) = decode_wav(hound::WavReader::open(&song).unwrap()).unwrap();
    let x = resample_stereo(&stereo, sr, 44_100);
    let start = (x.len() / 2 - SEGMENT) / 2;
    let w = &x[start * 2..(start + SEGMENT) * 2];
    let mono: Vec<f32> = w.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
    let mean = mono.iter().sum::<f32>() / mono.len() as f32;
    let std = (mono.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / mono.len() as f32).sqrt();
    let mut chunk = vec![0.0f32; 2 * SEGMENT];
    for i in 0..SEGMENT {
        chunk[i] = (w[2 * i] - mean) / std;
        chunk[SEGMENT + i] = (w[2 * i + 1] - mean) / std;
    }

    let mut cpu = session(&model, "cpu");
    let reference = run(&mut cpu, &chunk);

    let t = Instant::now();
    let mut s = session(&model, &config);
    let load = t.elapsed().as_secs_f32();
    let _ = run(&mut s, &chunk); // warm-up
    let t = Instant::now();
    let mut out = Vec::new();
    for _ in 0..3 {
        out = run(&mut s, &chunk);
    }
    let per = t.elapsed().as_secs_f32() / 3.0;

    let names = ["drums", "bass", "other", "vocals"];
    let snr: Vec<String> = (0..4)
        .map(|k| {
            let r = &reference[k * 2 * SEGMENT..(k + 1) * 2 * SEGMENT];
            let o = &out[k * 2 * SEGMENT..(k + 1) * 2 * SEGMENT];
            let sig: f64 = r.iter().map(|v| (*v as f64).powi(2)).sum();
            let err: f64 = r.iter().zip(o).map(|(a, b)| ((a - b) as f64).powi(2)).sum();
            format!("{} {:5.1}", names[k], 10.0 * (sig / err.max(1e-20)).log10())
        })
        .collect();
    println!("{config:28} load {load:5.1}s  {per:5.2}s/window  SNR dB: {}", snr.join("  "));
}
