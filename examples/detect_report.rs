//! Runs tempo detection over a folder of WAVs and compares it with tempos
//! written in the file names. Read-only.
//!
//! cargo run --release --example detect_report -- /path/to/folder

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempo_sampler::bpm::{self, NameBpm};

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
    let rows = Arc::new(Mutex::new(Vec::new()));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let (queue, rows) = (queue.clone(), rows.clone());
            std::thread::spawn(move || loop {
                let Some(path) = queue.lock().unwrap().pop() else { break };
                let name = path.file_stem().unwrap().to_string_lossy().into_owned();
                let name_bpm = bpm::bpm_from_name(&name);
                let (stereo, sr) = match tempo_sampler::audio_file::decode(&path) {
                    Ok(x) => x,
                    Err(e) => {
                        rows.lock().unwrap().push((name, 0.0, name_bpm, None, format!("decode: {e}")));
                        continue;
                    }
                };
                let mono: Vec<f32> = stereo.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
                let secs = mono.len() as f32 / sr as f32;
                let det = bpm::detect_bpm(&mono, sr);
                let info = match bpm::analyse(&mono, sr) {
                    Some(a) => {
                        let bar = a.bar_candidates.iter().copied().max_by(|x, y| x.1.total_cmp(&y.1));
                        match bar {
                            Some((b, sc)) => format!("free {:.1}@{:.3} bar {:.1}@{:.3}", a.free.0, a.free.1, b, sc),
                            None => format!("free {:.1}@{:.3}", a.free.0, a.free.1),
                        }
                    }
                    None => String::new(),
                };
                rows.lock().unwrap().push((name, secs, name_bpm, det, info));
            })
        })
        .collect();
    workers.into_iter().for_each(|w| w.join().unwrap());

    let mut rows = rows.lock().unwrap().clone();
    rows.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let (mut exact, mut octave, mut wrong, mut none) = (0, 0, 0, 0);
    for (name, secs, name_bpm, det, err) in &rows {
        let named = match name_bpm {
            Some(NameBpm::Explicit(b)) => format!("{b}"),
            Some(NameBpm::Bare(b)) => format!("{b}?"),
            None => "-".into(),
        };
        let verdict = match (name_bpm, det) {
            (Some(NameBpm::Explicit(n)), Some(d)) => {
                if bpm::close(*d, *n, 0.02) {
                    exact += 1;
                    "ok"
                } else if bpm::close(*d, n * 2.0, 0.02) || bpm::close(*d, n / 2.0, 0.02) {
                    octave += 1;
                    "octave"
                } else {
                    wrong += 1;
                    "WRONG"
                }
            }
            (Some(NameBpm::Explicit(_)), None) => {
                none += 1;
                "none"
            }
            _ => "",
        };
        let det = det.map(|d| format!("{d}")).unwrap_or("-".into());
        let short: String = name.chars().take(60).collect();
        println!("{short:60} {secs:7.1}s  name {named:>7}  detected {det:>6}  {verdict} {err}");
    }
    println!(
        "\nfiles with an explicit tempo in the name: exact {exact}, half/double {octave}, wrong {wrong}, nothing detected {none}"
    );
    let without: Vec<_> = rows.iter().filter(|r| !matches!(r.2, Some(NameBpm::Explicit(_)))).collect();
    let detected = without.iter().filter(|r| r.3.is_some()).count();
    println!("files without one: {} of {} got a tempo", detected, without.len());
}
