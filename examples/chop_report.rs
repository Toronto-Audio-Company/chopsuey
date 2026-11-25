//! Runs tempo detection and chop analysis on every song (> 60 s) in a
//! folder and prints what it finds. Read-only.
//!
//! cargo run --release --example chop_report -- /path/to/folder

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempo_sampler::bpm::{self, NameBpm};
use tempo_sampler::chops::analyse_song;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, out),
            Ok(t) if t.is_file() && tempo_sampler::audio_file::is_supported(&p) => {
                out.push(p)
            }
            _ => {}
        }
    }
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("folder argument"));
    let mut files = Vec::new();
    walk(&dir, &mut files);
    let queue = Arc::new(Mutex::new(files));
    let out = Arc::new(Mutex::new(Vec::new()));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let (queue, out) = (queue.clone(), out.clone());
            std::thread::spawn(move || loop {
                let Some(path) = queue.lock().unwrap().pop() else { break };
                let Ok((secs, _)) = tempo_sampler::audio_file::probe(&path) else { continue };
                if secs < 60.0 {
                    continue;
                }
                let Ok((stereo, sr)) = tempo_sampler::audio_file::decode(&path) else { continue };
                let mono: Vec<f32> = stereo.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
                let name = path.file_stem().unwrap().to_string_lossy().into_owned();
                let tempo = match bpm::bpm_from_name(&name) {
                    Some(NameBpm::Explicit(b)) => Some(b),
                    _ => bpm::detect_bpm(&mono, sr),
                };
                let Some(tempo) = tempo else { continue };
                let t0 = std::time::Instant::now();
                let a = analyse_song(&mono, sr, tempo);
                let ms = t0.elapsed().as_millis();
                let mut text = format!(
                    "{:60} {:6.1}s {:6.1} BPM  {} beats {} bars  {} chops  ({ms} ms)\n",
                    name.chars().take(60).collect::<String>(),
                    mono.len() as f32 / sr as f32,
                    tempo,
                    a.beats.len(),
                    a.bar_starts.len(),
                    a.chops.len()
                );
                for c in &a.chops {
                    text += &format!(
                        "    bar {:3} x{}  {:6.2}s  {:6.2} BPM  score {:.3}\n",
                        c.first_bar + 1,
                        c.bars,
                        c.start as f32 / sr as f32,
                        c.bpm,
                        c.score
                    );
                }
                out.lock().unwrap().push((name.to_lowercase(), text, a.chops.iter().map(|c| c.score).collect::<Vec<_>>()));
            })
        })
        .collect();
    workers.into_iter().for_each(|w| w.join().unwrap());
    let mut out = out.lock().unwrap().clone();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, text, _) in &out {
        print!("{text}");
    }
    let all: Vec<f32> = out.iter().flat_map(|o| o.2.iter().copied()).collect();
    let clean = all.iter().filter(|&&s| s >= 0.8).count();
    let none = out.iter().filter(|o| o.2.is_empty()).count();
    println!("\n{} songs, {} chops ({} clean >= 0.8), {} songs with no chops", out.len(), all.len(), clean, none);
}
