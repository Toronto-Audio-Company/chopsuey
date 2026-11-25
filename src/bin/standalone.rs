//! Runs chopsuey as a standalone app (no DAW), for development and
//! screenshots: `cargo run --release --bin standalone -- --backend dummy`.
//! Set TEMPO_SAMPLER_DEMO=<part of a name> to select that loop or song on launch.

fn main() {
    nih_plug::nih_export_standalone::<tempo_sampler::TempoSampler>();
}
