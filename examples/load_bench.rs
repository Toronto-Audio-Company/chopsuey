//! Times opening the library the way the plugin does: load settings, list
//! and read every file (pass 1, the list appears), then detect tempos that
//! aren't cached (pass 2). Runs against a copy of the real settings so they
//! aren't touched.
//!
//! cargo run --release --example load_bench            # warm: detection cached
//! cargo run --release --example load_bench -- --cold  # no detection cache

use std::sync::Arc;
use std::time::{Duration, Instant};

use tempo_sampler::library::Library;

fn main() {
    let cold = std::env::args().any(|a| a == "--cold");
    let home = std::env::var("HOME").unwrap();
    let real = format!("{home}/Library/Application Support/Tempo Sampler/library.json");
    let scratch = std::env::temp_dir().join(format!("tempo-sampler-bench-{}.json", std::process::id()));
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(&real).expect("no library settings")).unwrap();
    if cold {
        config["detected"] = serde_json::json!({});
        config["scan_cache"] = serde_json::json!({});
    }
    std::fs::write(&scratch, serde_json::to_vec(&config).unwrap()).unwrap();
    // SAFETY: single-threaded at this point.
    unsafe { std::env::set_var("TEMPO_SAMPLER_CONFIG", &scratch) };

    let t0 = Instant::now();
    let lib = Arc::new(Library::default());
    lib.ensure_loaded();
    let mut first_list = None;
    let mut listed = None;
    loop {
        let (n, scanning) = {
            let st = lib.state.lock().unwrap();
            (st.entries.len(), st.scanning)
        };
        if n > 1 && first_list.is_none() {
            first_list = Some((t0.elapsed(), n - 1));
        }
        if n > 1 && listed.is_none() && lib.state.lock().unwrap().entries.iter().all(|e| e.source != tempo_sampler::library::BpmSource::Pending) {
            listed = Some(t0.elapsed());
        }
        if !scanning && t0.elapsed() > Duration::from_millis(50) && first_list.is_some() {
            break;
        }
        if t0.elapsed() > Duration::from_secs(600) {
            println!("timed out");
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let done = t0.elapsed();
    let (shown_at, count) = first_list.unwrap_or_default();
    println!(
        "{}: {count} samples listed after {:.0} ms, tempos complete after {:.0} ms, scan done after {:.0} ms",
        if cold { "cold" } else { "warm" },
        shown_at.as_secs_f64() * 1000.0,
        listed.unwrap_or(done).as_secs_f64() * 1000.0,
        done.as_secs_f64() * 1000.0
    );

    // Open again with the settings the first scan saved (as the next session
    // would): the list should come straight from the scan cache.
    let t1 = Instant::now();
    let lib = Arc::new(Library::default());
    lib.ensure_loaded();
    while lib.state.lock().unwrap().entries.len() <= 1 && t1.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_micros(200));
    }
    let shown = t1.elapsed();
    while lib.state.lock().unwrap().scanning && t1.elapsed() < Duration::from_secs(600) {
        std::thread::sleep(Duration::from_millis(2));
    }
    println!(
        "next open: {} samples listed after {:.1} ms, rescan done after {:.0} ms",
        lib.state.lock().unwrap().entries.len() - 1,
        shown.as_secs_f64() * 1000.0,
        t1.elapsed().as_secs_f64() * 1000.0
    );
    let _ = std::fs::remove_file(&scratch);
}
