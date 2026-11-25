# Tempo Sampler

A VST3 plugin (Rust, nih-plug + vizia) that reads Ableton's tempo, stretches loops to match it, previews them in sync, and lets you drag the stretched WAV straight into Ableton.

## Build and install (macOS)

The stem separation model isn't in git (166 MB). Download it once:

```bash
mkdir -p models && curl -L -o models/htdemucs_fp16weights.onnx https://huggingface.co/StemSplitio/htdemucs-onnx/resolve/main/htdemucs_fp16weights.onnx
```

Then test, bundle (with the model and the WebGPU library `libwebgpu_dawn.dylib` inside the VST3), sign and install:

```bash
scripts/install.sh
```

In Ableton: Settings → Plug-Ins → turn on "Use VST3 Plug-in Custom Folder" or the system folder, then Rescan. Restart Live after reinstalling a changed build (Live keeps the old binary loaded).

Tempo Sampler is an instrument: put it on a MIDI track (it appears under Plug-Ins → Instruments). Click and hold a clip to hear it from its start; let go to stop.

**MIDI pads:** arm the track and play pads like a Drum Rack. For a song, C1 (note 36) plays the first chop tile of the bar length shown, the next note the second tile, and so on; for a loop or short sample, any pad plays it. A clip plays from its start while the pad is held. The tiles are rendered ahead at Live's tempo, so pads start instantly.

## Roadmap

1. **Host tempo display** (done): read tempo, play state, position and time signature in `process()`, share them through atomics, poll them in the GUI.
2. **Test loop** (done): `cargo run --example make_test_loop` writes `assets/test_loop_120bpm.wav` (2 bars, 120 BPM), embedded with `include_bytes!`.
3. **Pitch-preserving stretch** (done): Signalsmith Stretch in `src/loops.rs`, run on nih-plug's background thread 150 ms after the tempo settles. `cargo test --release -- --ignored write_previews` writes listenable renders to `target/stretch-preview/`.
4. **Preview** (done): plays the stretched buffer from its start while held (mouse or MIDI pad), looping, at the project tempo. "Show WAV in Finder" exports to `~/Music/Tempo Sampler/`.
5. **Drag out to Ableton** (built, macOS): start a native macOS drag (AppKit `NSDraggingSession` via `objc2`) of the exported WAV. vizia/baseview cannot drag files out of the window, so this part is custom code.
6. **Library browser** (built): "Add folder…" picks folders (scanned recursively for WAV, AIFF, FLAC, MP3, AAC/M4A, ALAC, Ogg Vorbis and CAF; decoded by `src/audio_file.rs` with hound for WAV and Symphonia for the rest, gapless so MP3/AAC loops stay on the beat). Tempo comes from a manual ½×/2× fix, else the file name ("124bpm", or a bare "_140_" if the length is whole bars), else detection (`src/bpm.rs`: loop length → candidate tempos, scored by onset autocorrelation). Folders, fixes and the detection cache live in `~/Library/Application Support/Tempo Sampler/library.json`. Clicking a loop selects and auditions it.
7. **Songs and chops** (built): any length works. Tempo detection uses spectral-flux onsets + autocorrelation (`src/bpm.rs`); short files snap to whole-bar tempos. Songs show at least 10 suggested chops per length (1, 2 and 4 bars, picked with tabs) as tiles with their own waveforms (`src/chops.rs`: dynamic-programming beat tracking, kick-based downbeats, 1/2/4-bar spans scored by repetition and seam continuity; clean distinct loops first, then filled shortest-first). Click a tile to loop it in sync; drag it into Ableton.

8. **Stems** (built): selecting a song splits it into drums / bass / other / vocals with HTDemucs on ONNX Runtime (`src/stems.rs`). The GPU (WebGPU on Metal, fp32, via Dawn) and 4 CPU threads take 7.8 s windows from one queue: ~0.18x real time on an M2 Max (a 2.5 min song in ~28 s; CPU alone took 56 s). Stems are cached as 16-bit WAVs in `~/Library/Caches/Tempo Sampler/stems` (least recently used deleted past 5 GB). Switches pick which stems play; previews and dragged WAVs contain only those.

   CoreML was rejected: every configuration that used the GPU produced wrong stems (0–20 dB SNR vs CPU), and the NeuralNetwork format failed to compile. `cargo run --release --example coreml_probe -- <model> <song.wav> <config>` reproduces this. A test checks GPU stems match CPU stems (> 40 dB SNR).

   **Licence:** the HTDemucs weights may be research-only (trained on MUSDB). Personal use only; swap the model before distributing the plugin.

UI: all colours, spacing and type are in `src/style.css` (light sheet on a dark frame; tokens, the 8px grid and type rules are listed at the top). To see the UI without Ableton, run the standalone app (`cargo run --release --bin standalone -- --backend dummy`) or capture it: `scripts/screenshot.sh out.png "<part of a sample name>" [seconds]`; `TEMPO_SAMPLER_DEMO_CHOP=<n>` and `TEMPO_SAMPLER_DEMO_STEMS=0011` preselect a chop tile and stem switches.

Dev tools (read-only): `cargo run --release --example detect_report -- <folder>` compares detected tempos with tempos in file names; `cargo run --release --example chop_report -- <folder>` lists the chops found in each song; `cargo run --release --example split_stems -- models/htdemucs_fp16weights.onnx <song> <out-dir>` splits one file and reports timing; `cargo run --release --example probe_files -- <file>...` checks that files decode and shows their detected tempo. `cargo run --release --example load_bench [-- --cold]` times opening the library (on a copy of your settings); `--cold` drops the tempo cache. Set `TEMPO_SAMPLER_TIMING=1` to print scan and window timings from the plugin or standalone app.
