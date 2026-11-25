//! Lists the one-shots found in each file of a folder (or in given files).
//! Read-only.
//!
//! cargo run --release --example one_shot_report -- <folder or files>...

use std::path::{Path, PathBuf};
use tempo_sampler::one_shots::find_one_shots;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if p.is_dir() {
            walk(&p, out);
        } else if tempo_sampler::audio_file::is_supported(&p) {
            out.push(p);
        }
    }
}

fn main() {
    let mut files = Vec::new();
    for arg in std::env::args().skip(1) {
        let p = PathBuf::from(arg);
        if p.is_dir() { walk(&p, &mut files) } else { files.push(p) }
    }
    files.sort();
    for path in files {
        let Ok((samples, sr)) = tempo_sampler::audio_file::decode(&path) else { continue };
        let mono: Vec<f32> = samples.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
        let t = std::time::Instant::now();
        let shots = find_one_shots(&mono, sr, 120.0);
        let ms = t.elapsed().as_secs_f32() * 1000.0;
        let secs = mono.len() as f32 / sr as f32;
        let lens: Vec<String> = shots
            .iter()
            .map(|c| format!("{:.0}:{:04.1}+{:.1}", (c.start as f32 / sr as f32 / 60.0).floor(), (c.start as f32 / sr as f32) % 60.0, (c.end - c.start) as f32 / sr as f32))
            .collect();
        println!("{:>3} shots  {:>5.0}s  {ms:>4.0}ms  {}", shots.len(), secs, path.file_name().unwrap().to_string_lossy());
        println!("      {}", lens.join("  "));
    }
}
