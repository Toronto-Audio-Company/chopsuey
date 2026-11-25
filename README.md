# Tempo Sampler

A VST3 plugin (Rust, nih-plug + vizia) that reads Ableton's tempo, stretches loops to match it, previews them in sync, and lets you drag the stretched WAV straight into Ableton.

## Build and install (macOS)

```bash
cargo xtask bundle tempo_sampler --release
rm -rf ~/Library/Audio/Plug-Ins/VST3/"Tempo Sampler.vst3"
cp -R target/bundled/"Tempo Sampler.vst3" ~/Library/Audio/Plug-Ins/VST3/
```

In Ableton: Settings → Plug-Ins → turn on "Use VST3 Plug-in Custom Folder" or the system folder, then Rescan. Restart Live after reinstalling a changed build (Live keeps the old binary loaded).

## Roadmap

1. **Host tempo display** (done): read tempo, play state, position and time signature in `process()`, share them through atomics, poll them in the GUI.
2. **Test loop** (done): `cargo run --example make_test_loop` writes `assets/test_loop_120bpm.wav` (2 bars, 120 BPM), embedded with `include_bytes!`.
3. **Pitch-preserving stretch** (done): Signalsmith Stretch in `src/loops.rs`, run on nih-plug's background thread 150 ms after the tempo settles. `cargo test --release -- --ignored write_previews` writes listenable renders to `target/stretch-preview/`.
4. **Synced preview** (done): plays the stretched buffer locked to Ableton's beat position; free-runs while stopped. "Show WAV in Finder" exports to `~/Music/Tempo Sampler/`.
5. **Drag out to Ableton** (built, macOS): start a native macOS drag (AppKit `NSDraggingSession` via `objc2`) of the exported WAV. vizia/baseview cannot drag files out of the window, so this part is custom code.
6. **Library browser**: a folder of loops with BPM metadata, list view, search.
