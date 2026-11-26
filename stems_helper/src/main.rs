//! `chopsuey-stems`: splits one audio file into stems for the plugin.
//!
//!     chopsuey-stems --model <htdemucs.onnx> --input <song> --out <cache dir>
//!
//! Writes the four stems into the cache folder (`tempo_sampler::stems::
//! save_cached`) where the plugin then reads them. Talks to the plugin over
//! stdout, one line each:
//!
//!     devices gpu+cpu     which hardware is running the model
//!     progress 0.42       0..1, a few times a second
//!     error <message>     a sentence fit to show the user (then exit 1)
//!     done                stems saved (then exit 0)
//!
//! The plugin keeps our stdin open. When it closes (the plugin cancelled,
//! was removed, or the host quit or crashed) we exit at once, so a split
//! never outlives the host.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use stems_helper::separator::{Separator, PLUGIN_ENGINE};

fn main() {
    let code = match run() {
        Ok(()) => {
            say("done");
            0
        }
        Err(e) => {
            say(&format!("error {}", e.replace('\n', " ")));
            1
        }
    };
    std::process::exit(code);
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn run() -> Result<(), String> {
    let (mut model, mut input, mut out) = (None, None, None);
    let mut args = std::env::args_os().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().map(PathBuf::from);
        match flag.to_str() {
            Some("--model") => model = value,
            Some("--input") => input = value,
            Some("--out") => out = value,
            _ => return Err(format!("unknown argument {flag:?}")),
        }
    }
    let (Some(model), Some(input), Some(out)) = (model, input, out) else {
        return Err("usage: chopsuey-stems --model <onnx> --input <audio> --out <dir>".into());
    };

    // Exit as soon as the plugin goes away.
    std::thread::spawn(|| {
        let mut sink = String::new();
        let mut stdin = std::io::stdin().lock();
        while stdin.read_line(&mut sink).is_ok_and(|n| n > 0) {
            sink.clear();
        }
        std::process::exit(2);
    });
    // Background priority: the host's audio threads come first.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) };

    let (stereo, sample_rate) = tempo_sampler::audio_file::decode(&input)
        .map_err(|e| format!("Couldn't read {}: {e}", input.display()))?;
    let mut separator = Separator::load(&model, PLUGIN_ENGINE)?;
    say(&format!("devices {}", separator.devices()));
    let mut last = -1.0f32;
    let stems = separator
        .separate(
            &stereo,
            sample_rate,
            |p| {
                if p - last >= 0.005 || p >= 1.0 {
                    last = p;
                    say(&format!("progress {p:.4}"));
                }
            },
            || false,
        )?
        .ok_or("cancelled")?;
    tempo_sampler::stems::save_cached(&out, &stems)
}
