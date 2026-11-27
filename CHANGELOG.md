# Changelog

## Unreleased

### New
- Loop and chop previews start on the beat while Live is playing, and follow the transport when it jumps. The new **Sync preview** parameter turns this off.
- chopsuey checks once a day for a newer version. When there is one, a line at the bottom of the window says so and opens the download page.
- chopsuey keeps a log in `~/Library/Logs/chopsuey`, to send along with a bug report.
- If a sample a set uses has moved, chopsuey looks for it in your library by name and size. If it can't find it, it says where the sample used to be.

### More reliable
- A bug in chopsuey no longer quits Live. If the sound engine hits one, chopsuey goes quiet and says so. Anything else that goes wrong only stops that one action. Either way, the details go to the log.
- Stem splitting runs in its own process. A crash in the splitter or the graphics driver can't take Live down, and its memory is freed when a split finishes.
- Several chopsuey instances, and several hosts, no longer overwrite each other's favourites and folders. A damaged settings file is kept to one side instead of being replaced with empty settings.
- Sets record a format version, and saved chops still come back after chop detection improves.
- Fixed a crash while finding one-shots in some songs.
- The installer is now 178 MB instead of 466 MB. The stem splitter is installed once for every format instead of inside each one.
- On Intel Macs, stem splitting says it needs Apple silicon instead of failing.

### Changed
- The vendor is now Toronto Audio Company. Sets made with earlier builds still find the plugin in VST3 hosts. CLAP hosts list it as a new plugin.
- Cached stems are split once more after this update. Their folder names now stay the same across Rust versions.
