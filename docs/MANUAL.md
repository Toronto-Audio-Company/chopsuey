# chopsuey manual

chopsuey is a sample browser for your DAW. It reads your set's tempo, stretches loops and songs to match it, chops songs into bars and one-shots, splits them into stems, and lets you drag the result into your set.

## Installing

1. Run the installer. It puts chopsuey in `/Library/Audio/Plug-Ins` as VST3, AU and CLAP.
2. Open Live. Settings → Plug-Ins → turn on the system folder (or Audio Units), then Rescan.

chopsuey is an instrument: put it on a MIDI track. In Live it's under Plug-Ins → Instruments → Toronto Audio Company.

## Your library

- **Add folder** (bottom left): adds a folder of samples. chopsuey scans its subfolders too.
- **Each sample shows its tempo, key and length**, for example "88 BPM · Am · 2:18". The key comes from the file name if it's there; otherwise chopsuey detects it.
- **Search:** matches names, folders and keys (`am`, `f#m`). A range like `90-100` filters by tempo, and words combine: `90-100 am`.
- **Favourites:** the ☆ next to a sample's title stars it. **Favourites** in the sidebar shows only starred samples.
- **½× 1× 2×:** use this when a tempo was detected at half or double speed.

## Previewing

- **Click and hold a sample or a tile** to hear it. Let go to stop.
- **While Live is playing,** a loop starts on the beat, in time with your set, the way a launched clip does. Turn this off with the **Sync preview** parameter.
- **Loudness:** previews play at matched loudness. Turn this off with **Match loudness**. Dragged files are never changed.

## Songs, chops and one-shots

- **Songs are chopped into bars.** Choose **1, 2 or 4 bars** to see the chops, then click a tile to hear it.
- **One shots** shows single sounds instead: phrases, shouts, horn blasts, anything with a pause on either side. They play at their own speed, once.
- **Stems:** the Drums, Bass, Other and Vocals switches play only those parts of a song. Splitting a song into stems takes a little while the first time; after that it's instant.

## Playing pads

Arm the track and play pads like a Drum Rack.

- For a song, C1 (note 36) plays the first chop tile, the next note the second tile, and so on.
- For a loop or short sample, any pad plays it.
- Pads start on the exact sample of the note, at its velocity.
- **Pad mode Mono:** a new pad cuts off the last one.
- **Pad mode Poly:** up to 8 pads can overlap.

## Pitch

**st** (±48 semitones) and **ct** (±50 cents) shift the pitch without changing the tempo, like a clip's Transpose and Detune in Live.

- Drag up or down on a box, or scroll over it, to change it. Double-click to reset.
- The shift applies to previews, pads and dragged files.

## Dragging into your set

Drag any sample, chop or one-shot from chopsuey into your set.

- **Format:** files are 24-bit WAVs at your set's sample rate.
- **Tempo:** they carry their tempo, so Live warps them correctly.
- **Loop points:** loops carry loop points.
- **One-shots** carry no tempo, so Live doesn't warp them.
- **Where they're saved:** a copy is saved in `~/Music/chopsuey`. The ↗ button next to the title shows it in Finder.

## Saved with your set

When you reopen a set, these come back, pads included:
- the selected sample
- the chop
- the bar length
- the stem switches

If you've moved the sample's folder, chopsuey finds the sample again as long as its new folder is in your library. If it can't find it, the sheet says where the sample used to be.

## Automation

You can automate these parameters or MIDI-map them in Live:
- the stem switches
- chop length
- tempo (½× 1× 2×)
- Transpose and Detune
- Pad mode
- Match loudness
- Sync preview

## Updates and help

- **Updates:** chopsuey checks once a day for a newer version. When there is one, a line at the bottom of the window says **chopsuey x.y.z is out**. Click it to open the download page. The check sends nothing about you or your computer.
- **Reporting a problem:** chopsuey's log is in `~/Library/Logs/chopsuey`. Send `chopsuey.log` with a sentence about what you were doing.
- **If chopsuey ever goes silent and says it stopped after an internal error,** your set is safe. Reopen the set to start it again, and please send the log.
