# chopsuey

A VST3 instrument (Rust, nih-plug + vizia), formerly "Tempo Sampler", that reads Ableton's tempo, stretches loops to match it, previews them in sync, and lets you drag the stretched WAV straight into Ableton.

## Build and install (macOS)

One command downloads the stem separation model (166 MB, first time only), runs the tests, builds the plugin and installs it where Ableton looks for VST3s:

```bash
scripts/install.sh
```

You need Rust ([rustup.rs](https://rustup.rs)). Run it again after pulling changes; the model is only downloaded once (to `models/`, which isn't in git).

In Ableton: Settings → Plug-Ins → turn on "Use VST3 Plug-in Custom Folder" or the system folder, then Rescan. Restart Live after reinstalling a changed build (Live keeps the old binary loaded).

chopsuey is an instrument: put it on a MIDI track (it appears under Plug-Ins → Instruments). Click and hold a clip to hear it from its start; let go to stop.

**MIDI pads:** arm the track and play pads like a Drum Rack. For a song, C1 (note 36) plays the first chop tile of the bar length shown, the next note the second tile, and so on; for a loop or short sample, any pad plays it. A clip plays from its start while the pad is held, starting on the exact sample of the note, at its velocity. The tiles are rendered ahead at Live's tempo, so pads start instantly. **Pad mode** chooses Mono (a new pad cuts off the last, like a choke group) or Poly (up to 8 overlap).

**Saved with your set:** the selected sample, chop, bar length and stem switches come back when you reopen the set, pads included.

**Automation and MIDI mapping:** the stem switches, chop length, tempo (½×/1×/2×), pad mode and Match loudness are plugin parameters: automate them or map them in Live (Configure, or MIDI Map mode).

**Browsing:** each sample shows its tempo and key ("88 BPM · Am · 2:18"; the key from the file name if it's there, else detected). Search matches names, folders and keys (`am`, `f#m`), and a range like `90-100` filters by tempo; words combine (`90-100 am`). Star a sample with the ☆ next to its title and show only starred ones with **Favourites**. Previews play at matched loudness (turn off with the Match loudness parameter); dragged files are untouched.

**Dragged WAVs** are 24-bit at Live's sample rate and carry their tempo (an `acid` chunk) and, for loops, loop points (`smpl`), like sample-pack loops.

## Stem model licence

The HTDemucs weights may be research-only (trained on MUSDB). Personal use only; swap the model before distributing the plugin.

## Development

UI: all colours, spacing and type are in `src/style.css` (light sheet on a dark frame; tokens, the 8px grid and type rules are listed at the top). To see the UI without Ableton, run the standalone app (`cargo run --release --bin standalone -- --backend dummy`) or capture it: `scripts/screenshot.sh out.png "<part of a sample name>" [seconds]`; `TEMPO_SAMPLER_DEMO_CHOP=<n>` and `TEMPO_SAMPLER_DEMO_STEMS=0011` preselect a chop tile and stem switches.

Dev tools (read-only): `cargo run --release --example detect_report -- <folder>` compares detected tempos with tempos in file names; `cargo run --release --example chop_report -- <folder>` lists the chops found in each song; `cargo run --release --example split_stems -- models/htdemucs_fp16weights.onnx <song> <out-dir>` splits one file and reports timing; `cargo run --release --example probe_files -- <file>...` checks that files decode and shows their detected tempo. `cargo run --release --example key_bench -- <folder>` scores key-detection settings against keys written in file names. `cargo run --release --example load_bench [-- --cold]` times opening the library (on a copy of your settings); `--cold` drops the tempo cache. Set `TEMPO_SAMPLER_TIMING=1` to print scan and window timings from the plugin or standalone app.
