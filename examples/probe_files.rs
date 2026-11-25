//! Probes and decodes audio files and reports length, sample rate and the
//! detected tempo. Read-only.
//!
//! cargo run --release --example probe_files -- <file>...

fn main() {
    for arg in std::env::args().skip(1) {
        let path = std::path::Path::new(&arg);
        let name = path.file_name().unwrap().to_string_lossy();
        let t = std::time::Instant::now();
        let probed = tempo_sampler::audio_file::probe(path);
        let decoded = tempo_sampler::audio_file::decode(path);
        match (probed, decoded) {
            (Ok((secs, sr)), Ok((stereo, dsr))) => {
                let mono: Vec<f32> = stereo.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
                let decoded_secs = mono.len() as f32 / dsr as f32;
                let peak = mono.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                let bpm = tempo_sampler::bpm::detect_bpm(&mono, dsr);
                println!(
                    "{name}: header {secs:.2}s @ {sr} Hz, decoded {decoded_secs:.2}s, peak {peak:.2}, tempo {bpm:?} ({:.1}s)",
                    t.elapsed().as_secs_f32()
                );
            }
            (p, d) => println!("{name}: FAILED probe {:?} decode {:?}", p.err(), d.err()),
        }
    }
}
