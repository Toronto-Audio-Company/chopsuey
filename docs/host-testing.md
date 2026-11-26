# Host test pass

The validators (`scripts/validate.sh`) catch API mistakes. These checks catch what they can't, and need a person at a host. Do the whole list in Live 12 before each release, and the quick list in every other host you support.

## Hosts

| Host | Formats | Priority |
|---|---|---|
| Ableton Live 12 (Lite and Suite) | VST3, AU | Every release |
| Ableton Live 11 | VST3, AU | Every release |
| Logic Pro / GarageBand | AU | Every release |
| Bitwig Studio | CLAP, VST3 | Major releases |
| Reaper | VST3, CLAP, AU | Major releases (it is the strictest about threading) |
| FL Studio (Mac) | VST3, AU | Major releases |

Use both Apple Silicon and Intel (or Live under Rosetta) for the universal build.

## Whole list

**Loading**
- [ ] It appears under Instruments with vendor Toronto Audio Company, and the right version is shown.
- [ ] Insert it, remove it, and insert it again. Insert 8 instances at once. Live stays responsive.
- [ ] Open a set saved with the previous release. The sample, chop, bar length and stems all come back.

**Audio settings**
- [ ] Sample rate at 44.1, 48 and 96 kHz: previews and pads play at the right pitch and tempo. Change the rate while the set is open.
- [ ] Buffer size at 32, 128, 512 and 2048: no clicks or dropouts while previewing or playing pads.

**Transport**
- [ ] Change the tempo while previewing: it re-stretches and keeps its place.
- [ ] Press play: a loop preview lands on the beat. Loop a section of the arrangement: the preview follows the jump.
- [ ] Turn off Sync preview: the preview starts from its top.

**Export and stems**
- [ ] Freeze a track, then export audio (offline bounce). The pads render in the bounce at the right time.
- [ ] Drag a loop, a chop and a one-shot into the set. Their tempo and warping are right.
- [ ] Split a song into stems, then close the set mid-split. No `chopsuey-stems` process is left running (check Activity Monitor).
- [ ] Split while 3 instances each ask for stems: they take turns and Live stays smooth.

**Saved state**
- [ ] Move the sample's folder, add the new folder, and reopen the set: the sample is found again. Remove the folder entirely: the sheet says what's missing.
- [ ] Two instances: star a sample in one and a different one in the other. Both stars survive a restart.

**Licence**
- [ ] A fresh Mac shows "Trial · 14 days left".
- [ ] Paste a key: activation works. A bad key gives a clear message. With the trial over, dragging out asks for a key and previews still play.

**Other**
- [ ] Automate and MIDI-map Transpose, the stem switches and Sync preview.
- [ ] Resize the window from the corner. Reopen the set: the size is kept.
- [ ] Report a problem reveals the log.

## Quick list for other hosts
- [ ] Loading: it scans, inserts, and plays MIDI pads.
- [ ] Previews and dragging out work.
- [ ] Saving and reopening a project brings everything back.
- [ ] Changing the tempo re-stretches.
- [ ] An offline bounce renders the pads.
- [ ] Closing the project leaves no `chopsuey-stems` process running.
