//! Scores key-detection variants against keys written in file names.
//!
//! cargo run --release --example key_bench -- /path/to/folder

use std::path::{Path, PathBuf};
use tempo_sampler::key::{detect_key_with, key_from_name, KeyOptions, Profiles};

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
    let dir = PathBuf::from(std::env::args().nth(1).expect("folder"));
    let mut files = Vec::new();
    walk(&dir, &mut files);
    let labelled: Vec<(String, Vec<f32>, u32)> = files
        .iter()
        .filter_map(|p| {
            let want = key_from_name(&p.file_stem()?.to_string_lossy())?;
            let (w, sr, _) = tempo_sampler::audio_file::decode_mono_window(p).ok()?;
            Some((want, w, sr))
        })
        .collect();
    println!("{} files with a key in the name", labelled.len());
    for profiles in [Profiles::Krumhansl, Profiles::Temperley, Profiles::Edma] {
        for (fmin, fmax) in [(65.0, 5000.0), (100.0, 2000.0), (50.0, 1000.0)] {
            for power in [1.0, 0.5, 0.0] {
                let opts = KeyOptions { profiles, fmin, fmax, power, min_confidence: -1.0 };
                let right = labelled.iter().filter(|(want, w, sr)| detect_key_with(w, *sr, opts).as_deref() == Some(want)).count();
                println!("{:9?} {:>4}-{:<5} power {power}: {right}/{}", profiles, fmin, fmax, labelled.len());
            }
        }
    }
}
