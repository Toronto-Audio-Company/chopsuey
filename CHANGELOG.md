# Changelog

## Unreleased

### New
- Loop and chop previews start on the beat while Live is playing, and follow the transport when it jumps. The new **Sync preview** parameter turns this off.
- **Licence and trial.** chopsuey runs fully for 14 days. After that, previews and pads still work, and dragging samples into your set needs a licence key, which you paste into the box at the bottom left of the sidebar.
- **Check for updates** is off unless you turn it on. When it's on, chopsuey checks once a day and tells you when a new version is out.
- **Report a problem** reveals chopsuey's log (`~/Library/Logs/chopsuey`) in Finder, so you can send it to support.
- If a sample a set uses has moved, chopsuey looks for it in your library by name and size. If it can't find it, it says where the sample used to be.

### More reliable
- A bug in chopsuey no longer quits Live. If the sound engine hits one, chopsuey goes quiet and says so. Anything else that goes wrong only stops that one action. Either way, the details go to the log.
- Stem splitting runs in its own process. A crash in the splitter or the graphics driver can't take Live down, and its memory is freed when a split finishes.
- Several chopsuey instances, and several hosts, no longer overwrite each other's favourites and folders. A damaged settings file is kept to one side instead of being replaced with empty settings.
- Sets record a format version, and saved chops still come back after chop detection improves.
- Fixed a crash while finding one-shots in some songs.

### Changed
- The vendor is now Toronto Audio Company. Sets made with earlier builds still find the plugin in VST3 hosts. CLAP hosts list it as a new plugin.
- Cached stems are split once more after this update. Their folder names now stay the same across Rust versions.
