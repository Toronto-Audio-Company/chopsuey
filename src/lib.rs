use atomic_float::{AtomicF32, AtomicF64};
use nih_plug::prelude::*;
use nih_plug_vizia::ViziaState;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub mod audio_file;
pub mod bpm;
pub mod chops;
pub mod dsp;
mod dialog;
mod drag;
pub mod library;
mod editor;
pub mod loops;
pub mod stems;

use library::Library;
use loops::{LoopAsset, RenderedLoop};

/// Wait this long after the tempo stops changing before re-stretching, so
/// dragging the tempo in Ableton doesn't queue up dozens of renders.
const RENDER_DEBOUNCE_SECS: f32 = 0.15;
/// Fade time for the preview on/off switch, to avoid clicks.
const PREVIEW_FADE_SECS: f32 = 0.01;

/// Transport info the audio thread writes and the GUI reads.
///
/// The audio thread must never block, so everything here is a lock-free atomic.
pub struct HostTransport {
    /// Project tempo in BPM. 0.0 until the host has told us anything.
    pub tempo: AtomicF32,
    pub playing: AtomicBool,
    /// Playhead position in quarter notes since the start of the song.
    pub pos_beats: AtomicF64,
    pub time_sig_numerator: AtomicI32,
    pub time_sig_denominator: AtomicI32,
}

impl Default for HostTransport {
    fn default() -> Self {
        Self {
            tempo: AtomicF32::new(0.0),
            playing: AtomicBool::new(false),
            pos_beats: AtomicF64::new(0.0),
            time_sig_numerator: AtomicI32::new(4),
            time_sig_denominator: AtomicI32::new(4),
        }
    }
}

/// The loop the user picked, and the tempo it was recorded at.
#[derive(Clone, PartialEq)]
pub struct Selection {
    pub key: String,
    /// `None` for the built-in test loop.
    pub path: Option<PathBuf>,
    pub name: String,
    pub bpm: f32,
    /// A chop of this song instead of the whole file.
    pub chop: Option<ChopSel>,
    /// Which stems to hear (bit i = `stems::STEM_NAMES[i]`). `ALL_STEMS`
    /// plays the original file; anything else needs the song's stems.
    pub stem_mask: u8,
}

/// Every stem on: the original audio.
pub const ALL_STEMS: u8 = 0b1111;

/// Where stem separation is for the selected song.
#[derive(Clone, Debug, PartialEq)]
pub enum StemState {
    Idle,
    /// Reading cached stems from disk.
    Loading,
    /// Separating; progress is in `Shared::stem_progress`.
    Splitting,
    Ready,
    Failed(String),
}

#[derive(Clone, PartialEq)]
pub struct ChopSel {
    /// Index in the song's chop list.
    pub index: usize,
    /// Number shown to the user, counted within chops of the same length.
    pub number: usize,
    pub chop: chops::Chop,
}

impl Selection {
    fn builtin() -> Self {
        Self {
            key: library::BUILTIN_KEY.into(),
            path: None,
            name: "Test Loop".into(),
            bpm: 120.0,
            chop: None,
            stem_mask: ALL_STEMS,
        }
    }

    /// Key of the audio this selection plays: the file or one chop of it,
    /// and which stems.
    pub fn render_key(&self) -> String {
        let mut key = match &self.chop {
            Some(c) => format!("{}#{}-{}", self.key, c.chop.start, c.chop.end),
            None => self.key.clone(),
        };
        if self.stem_mask != ALL_STEMS {
            key += &format!("|stems{:04b}", self.stem_mask);
        }
        key
    }

    /// " (drums+bass)" when only some stems are on, else "".
    pub fn stems_suffix(&self) -> String {
        if self.stem_mask == ALL_STEMS {
            return String::new();
        }
        let on: Vec<&str> = (0..4).filter(|i| self.stem_mask & (1 << i) != 0).map(|i| stems::STEM_NAMES[i]).collect();
        format!(" ({})", on.join("+"))
    }
}

/// A decoded audio file (interleaved stereo).
pub struct Source {
    pub samples: Arc<Vec<f32>>,
    pub sample_rate: u32,
}

impl Source {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }
}

/// The selected song, analysed: a waveform overview and suggested chops.
pub struct SongView {
    pub key: String,
    pub bpm: f32,
    pub frames: usize,
    pub sample_rate: u32,
    /// (min, max) of the mono signal in `OVERVIEW_POINTS` equal slices.
    pub overview: Vec<(f32, f32)>,
    pub bar_starts: Vec<usize>,
    pub chops: Vec<chops::Chop>,
    /// (min, max) waveform of each chop in `CHOP_OVERVIEW_POINTS` slices.
    pub chop_overviews: Vec<Vec<(f32, f32)>>,
}

const OVERVIEW_POINTS: usize = 1200;
const CHOP_OVERVIEW_POINTS: usize = 200;

/// (min, max) of the mono mix of interleaved stereo `x` in `points` equal
/// slices, without building the mono signal first. Same values as
/// `min_max_overview` on `(l + r) * 0.5`.
fn min_max_overview_stereo(x: &[f32], points: usize) -> Vec<(f32, f32)> {
    let frames = x.len() / 2;
    let bucket = frames.div_ceil(points).max(1);
    x.chunks(bucket * 2)
        .map(|c| {
            c.chunks_exact(2).fold((0.0f32, 0.0f32), |(lo, hi), f| {
                let v = (f[0] + f[1]) * 0.5;
                (lo.min(v), hi.max(v))
            })
        })
        .collect()
}

/// Like `min_max_overview_stereo` over the sum of the stems in `mask`,
/// frames `start..end`, without mixing them into a buffer first.
fn min_max_overview_mixed(stems: &stems::Stems, mask: u8, start: usize, end: usize, points: usize) -> Vec<(f32, f32)> {
    let frames = stems.stems[0].len() / 2;
    let (start, end) = (start.min(frames), end.min(frames));
    let on: Vec<&[f32]> = (0..4).filter(|i| mask & (1 << i) != 0).map(|i| &stems.stems[i][..]).collect();
    let bucket = (end - start).div_ceil(points).max(1);
    let mut out = Vec::with_capacity(points);
    let mut f = start;
    while f < end {
        let stop = (f + bucket).min(end);
        let (mut lo, mut hi) = (0.0f32, 0.0f32);
        for g in f..stop {
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for s in &on {
                l += s[g * 2];
                r += s[g * 2 + 1];
            }
            let v = (l + r) * 0.5;
            lo = lo.min(v);
            hi = hi.max(v);
        }
        out.push((lo, hi));
        f = stop;
    }
    out
}

/// (min, max) of `x` in `points` equal slices.
fn min_max_overview(x: &[f32], points: usize) -> Vec<(f32, f32)> {
    let bucket = x.len().div_ceil(points).max(1);
    x.chunks(bucket)
        .map(|c| c.iter().fold((0.0f32, 0.0f32), |(lo, hi), &v| (lo.min(v), hi.max(v))))
        .collect()
}
/// Files at least this long (and not whole-bar loops) get chop analysis.
pub const SONG_MIN_SECS: f32 = 20.0;
/// Non-loop files longer than this are stretched on their own thread so
/// they don't hold up quick renders (chops) queued behind them.
const LONG_RENDER_SECS: f32 = 20.0;
/// Seconds of a long render's start (in the original file) stretched and
/// played first, while the rest stretches.
const START_RENDER_SECS: f32 = 10.0;

/// State shared by the audio thread, the background render thread and the GUI.
pub struct Shared {
    pub transport: HostTransport,
    pub library: Arc<Library>,
    /// What the GUI wants playing. Read by the render thread.
    pub selection: Mutex<Selection>,
    /// Decoded audio of the selected file, by key.
    source: Mutex<Option<(String, Arc<Source>)>>,
    /// Render-thread cache of the selection's audio at its original tempo.
    loaded: Mutex<Option<(Selection, Arc<LoopAsset>)>>,
    /// Analysis of the selected song, if it is one. Check `key` before use.
    pub song_view: Mutex<Option<Arc<SongView>>>,
    /// Bumped when `song_view` changes, so the GUI redraws.
    pub song_view_version: AtomicU64,
    /// "key@bpm" of the chop analysis running now, if any.
    analysing: Mutex<Option<String>>,
    /// Chop length (bars) shown in the chop grid. Kept here so it survives
    /// closing the window.
    pub chop_length: AtomicUsize,
    /// The stem switches (bit i = `stems::STEM_NAMES[i]`). Kept across
    /// songs; applied once a song's stems are ready.
    pub stem_switches: AtomicU8,
    /// Separated stems of the selected song, by key.
    stems: Mutex<Option<(String, Arc<stems::Stems>)>>,
    /// Chop waveforms mixed from stems: (song key, mask, overviews).
    mixed_overviews: Mutex<Option<(String, u8, Arc<Vec<Vec<(f32, f32)>>>)>>,
    /// Separation status for `stem_state.0` (a song key).
    pub stem_state: Mutex<(String, StemState)>,
    /// 0..1 while splitting.
    pub stem_progress: AtomicF32,
    /// The running split: song key and its cancel flag.
    stem_job: Mutex<Option<(String, Arc<AtomicBool>)>>,
    /// Preview position within the selected song (0..1), or -1 when not playing.
    pub playhead: AtomicF32,
    /// Serialises publishing renders, which can finish on two threads.
    publish_lock: Mutex<()>,
    /// Id of the newest render request. Queued requests with an older id are
    /// skipped, since a newer tempo or loop superseded them.
    latest_request: AtomicU64,
    /// Id of the request the current render came from.
    rendered_request: AtomicU64,
    /// Tempo of the most recently finished render, 0.0 before the first one.
    pub rendered_bpm: AtomicF32,
    pub render_error: Mutex<Option<String>>,
    /// Whether the loop preview is playing. GUI-only state rather than a host
    /// parameter: it isn't saved with the set, doesn't create undo steps, and
    /// is switched off when the window closes or Ableton's transport stops.
    pub preview: AtomicBool,
    /// Latest render, for the GUI (export / drag). Never touched by the audio thread.
    current: Mutex<Option<Arc<RenderedLoop>>>,
    /// Waveform of `current`: (min, max) in `OVERVIEW_POINTS` slices.
    current_overview: Mutex<Arc<Vec<(f32, f32)>>>,
    /// The render most recently written to disk, and where.
    exported: Mutex<Option<(Arc<RenderedLoop>, PathBuf)>>,
    /// New renders travel to the audio thread through this lock-free queue...
    to_audio: Mutex<rtrb::Producer<Arc<RenderedLoop>>>,
    /// ...and the ones it replaces come back here, so their memory is freed
    /// on the render thread instead of the audio thread.
    garbage: Mutex<rtrb::Consumer<Arc<RenderedLoop>>>,
}

impl Shared {
    /// Allocates an id for a new render request. Realtime-safe.
    pub fn next_request(&self) -> u64 {
        self.latest_request.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// False while a requested render hasn't finished yet.
    pub fn is_up_to_date(&self) -> bool {
        self.rendered_request.load(Ordering::Relaxed) == self.latest_request.load(Ordering::Relaxed)
    }

    pub fn current(&self) -> Option<Arc<RenderedLoop>> {
        self.current.lock().unwrap().clone()
    }

    /// The current render and its waveform.
    pub fn current_with_overview(&self) -> Option<(Arc<RenderedLoop>, Arc<Vec<(f32, f32)>>)> {
        let _guard = self.publish_lock.lock().unwrap();
        Some((self.current()?, self.current_overview.lock().unwrap().clone()))
    }

    /// Runs on nih-plug's background thread.
    fn render(self: &Arc<Self>, id: u64, bpm: f32) {
        if self.latest_request.load(Ordering::Relaxed) != id {
            return; // superseded while queued
        }
        while self.garbage.lock().unwrap().pop().is_ok() {}

        let selection = self.selection.lock().unwrap().clone();
        let asset = match self.load(&selection) {
            Ok(asset) => asset,
            Err(e) => {
                *self.render_error.lock().unwrap() = Some(e);
                self.rendered_request.store(id, Ordering::Relaxed);
                return;
            }
        };
        *self.render_error.lock().unwrap() = None;

        let span = self.span_of(&selection);
        let secs = asset.frames() as f32 / asset.sample_rate as f32;
        if !asset.is_loop && secs > LONG_RENDER_SECS {
            // Long: stretch on its own thread so a chop clicked meanwhile
            // doesn't wait behind it. The first few seconds are stretched and
            // published first so playback starts at once; the full render
            // replaces them without a jump (same tempo, same start).
            // Stale results are dropped in publish().
            let shared = self.clone();
            std::thread::Builder::new()
                .name("tempo-sampler-song".into())
                .spawn(move || {
                    let current = || shared.latest_request.load(Ordering::Relaxed) == id;
                    let start_frames = (START_RENDER_SECS * asset.sample_rate as f32) as usize;
                    if current() && asset.frames() > start_frames * 2 {
                        let mut start = asset.stretch_start(bpm, start_frames);
                        let part = start_frames as f32 / asset.frames() as f32;
                        start.span = span.map(|(a, b)| (a, a + (b - a) * part));
                        shared.publish(id, bpm, start);
                    }
                    // Gives up between pieces once a newer request exists, so
                    // flicking through songs doesn't pile up stretches.
                    if let Some(mut rendered) = asset.stretch_to_cancellable(bpm, &|| !current()) {
                        rendered.span = span;
                        shared.publish(id, bpm, rendered);
                    }
                })
                .ok();
            // Find chops on another thread while it stretches (the whole
            // song plays without them), leaving this one free for the next
            // request (a chop click, say).
            if let Ok(src) = self.source(&selection) {
                let (shared, selection) = (self.clone(), selection.clone());
                std::thread::Builder::new()
                    .name("tempo-sampler-chops".into())
                    .spawn(move || shared.analyse_if_song(&selection, &src))
                    .ok();
            }
        } else {
            let mut rendered = asset.stretch_to(bpm);
            rendered.span = span;
            self.publish(id, bpm, rendered);
        }
        // Tiles show the mix of the stems that are on: work those waveforms
        // out here rather than in the GUI's next draw.
        if selection.stem_mask != ALL_STEMS {
            let _ = self.mixed_chop_overviews(selection.stem_mask);
        }
    }

    /// Makes a finished render current, unless a newer request exists. A
    /// partial render (a song's start) plays but doesn't count as done.
    fn publish(&self, id: u64, bpm: f32, rendered: RenderedLoop) {
        // Outside the lock: the GUI takes it to draw.
        let overview = Arc::new(min_max_overview_stereo(&rendered.samples, OVERVIEW_POINTS));
        let _guard = self.publish_lock.lock().unwrap();
        if self.latest_request.load(Ordering::Relaxed) != id {
            return; // superseded while stretching
        }
        // Free buffers the audio thread handed back (song renders publish
        // from their own thread, so render() alone may not keep up).
        while self.garbage.lock().unwrap().pop().is_ok() {}
        let partial = rendered.partial;
        let rendered = Arc::new(rendered);
        *self.current.lock().unwrap() = Some(rendered.clone());
        *self.current_overview.lock().unwrap() = overview;
        // If the queue is full the audio thread isn't running; the GUI still
        // has the render, and the next one will get through.
        let _ = self.to_audio.lock().unwrap().push(rendered);
        if !partial {
            self.rendered_bpm.store(bpm, Ordering::Relaxed);
            self.rendered_request.store(id, Ordering::Relaxed);
        }
    }

    /// Where the selection sits in its file, for the playhead.
    fn span_of(&self, selection: &Selection) -> Option<(f32, f32)> {
        let view = self.song_view.lock().unwrap().clone();
        let Some(view) = view.filter(|v| v.key == selection.key && v.frames > 0) else {
            // Not analysed (a loop, or a song still being analysed): the
            // whole file is playing.
            return selection.chop.is_none().then_some((0.0, 1.0));
        };
        let total = view.frames as f32;
        Some(match &selection.chop {
            Some(c) => (c.chop.start as f32 / total, c.chop.end as f32 / total),
            None => (0.0, 1.0),
        })
    }

    /// Decoded audio for a file, reusing the last one if it's the same file.
    fn source(&self, selection: &Selection) -> Result<Arc<Source>, String> {
        self.decoded(&selection.key, selection.path.as_deref(), &selection.name)
    }

    /// Decodes `path` (or the built-in loop), or returns it if it's the file
    /// decoded last. The lock is held while decoding, so the render and
    /// stems threads asking for the same file decode it once between them.
    fn decoded(&self, key: &str, path: Option<&std::path::Path>, name: &str) -> Result<Arc<Source>, String> {
        let mut cached = self.source.lock().unwrap();
        if let Some((k, src)) = cached.as_ref() {
            if k == key {
                return Ok(src.clone());
            }
        }
        let (samples, sample_rate) = match path {
            None => {
                let a = LoopAsset::test_loop();
                (a.samples, a.sample_rate)
            }
            Some(path) => {
                let (s, sr) = audio_file::decode(path).map_err(|e| format!("Couldn't read {name}: {e}"))?;
                (Arc::new(s), sr)
            }
        };
        let src = Arc::new(Source { samples, sample_rate });
        *cached = Some((key.to_string(), src.clone()));
        Ok(src)
    }

    /// The selection's audio at its original tempo (whole file or a chop),
    /// reusing the last one if unchanged.
    fn load(&self, selection: &Selection) -> Result<Arc<LoopAsset>, String> {
        if let Some((sel, asset)) = self.loaded.lock().unwrap().as_ref() {
            if sel == selection {
                return Ok(asset.clone());
            }
        }
        let src = self.source(selection)?;
        let asset = Arc::new(self.make_asset(selection, &src)?);
        *self.loaded.lock().unwrap() = Some((selection.clone(), asset.clone()));
        Ok(asset)
    }

    /// The selection's audio at its original tempo, cut from `src` (the
    /// whole file or a chop) and mixed from the enabled stems.
    fn make_asset(&self, selection: &Selection, src: &Source) -> Result<LoopAsset, String> {
        // The audio range, from the original or from the enabled stems.
        let (start, end) = match &selection.chop {
            Some(c) => (c.chop.start.min(src.frames()), c.chop.end.min(src.frames())),
            None => (0, src.frames()),
        };
        let samples = if selection.stem_mask == ALL_STEMS {
            if start == 0 && end == src.frames() {
                src.samples.clone() // the whole file: share it, don't copy
            } else {
                Arc::new(src.samples[start * 2..end * 2].to_vec())
            }
        } else {
            let stems = self.stems_for(&selection.key).ok_or("Stems aren't ready yet")?;
            Arc::new(stems::mix(&stems, selection.stem_mask, start, end))
        };
        let (name, bpm) = match &selection.chop {
            Some(c) => (format!("{} - {} bar chop {}", selection.name, c.chop.bars, c.number), c.chop.bpm),
            None => (selection.name.clone(), selection.bpm),
        };
        Ok(LoopAsset::new(
            &selection.render_key(),
            &(name + &selection.stems_suffix()),
            bpm,
            src.sample_rate,
            samples,
        ))
    }

    /// Runs chop analysis for a song (not a short loop) if it hasn't been
    /// done for this file and tempo yet.
    fn analyse_if_song(&self, selection: &Selection, src: &Source) {
        if selection.path.is_none() {
            return;
        }
        let secs = src.frames() as f32 / src.sample_rate as f32;
        if secs < SONG_MIN_SECS || bpm::is_loop(secs, selection.bpm) {
            return;
        }
        if let Some(view) = self.song_view.lock().unwrap().as_ref() {
            if view.key == selection.key && view.bpm == selection.bpm {
                return;
            }
        }
        // One analysis per file and tempo at a time (renders come in bursts).
        let job = format!("{}@{}", selection.key, selection.bpm);
        {
            let mut running = self.analysing.lock().unwrap();
            if running.as_deref() == Some(job.as_str()) {
                return;
            }
            *running = Some(job.clone());
        }
        let mono: Vec<f32> = src.samples.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
        let analysis = chops::analyse_song(&mono, src.sample_rate, selection.bpm);
        let overview = min_max_overview(&mono, OVERVIEW_POINTS);
        let chop_overviews = analysis
            .chops
            .iter()
            .map(|c| min_max_overview(&mono[c.start.min(mono.len())..c.end.min(mono.len())], CHOP_OVERVIEW_POINTS))
            .collect();
        *self.song_view.lock().unwrap() = Some(Arc::new(SongView {
            key: selection.key.clone(),
            bpm: selection.bpm,
            frames: src.frames(),
            sample_rate: src.sample_rate,
            overview,
            bar_starts: analysis.bar_starts,
            chops: analysis.chops,
            chop_overviews,
        }));
        self.song_view_version.fetch_add(1, Ordering::Relaxed);
        let mut running = self.analysing.lock().unwrap();
        if running.as_deref() == Some(job.as_str()) {
            *running = None;
        }
    }

    /// The selected song's stems, if they're ready.
    pub fn stems_for(&self, key: &str) -> Option<Arc<stems::Stems>> {
        self.stems.lock().unwrap().as_ref().filter(|(k, _)| k == key).map(|(_, s)| s.clone())
    }

    /// Stem mask to use for `key` right now: the switches if its stems are
    /// ready, otherwise the original audio.
    pub fn effective_stem_mask(&self, key: &str) -> u8 {
        if self.stems_for(key).is_some() {
            self.stem_switches.load(Ordering::Relaxed)
        } else {
            ALL_STEMS
        }
    }

    /// Requests a render of the current selection at the host tempo.
    fn render_now(self: &Arc<Self>) {
        let tempo = self.transport.tempo.load(Ordering::Relaxed);
        if tempo > 0.0 {
            let id = self.next_request();
            self.render(id, tempo);
        }
    }

    /// Makes sure stems for this song exist: loads them from the cache or
    /// separates them on a background thread. Cancels a split running for
    /// a different song. Called when a song is selected.
    pub fn request_stems(self: &Arc<Self>, key: &str, path: &std::path::Path) {
        if self.stems_for(key).is_some() {
            *self.stem_state.lock().unwrap() = (key.to_string(), StemState::Ready);
            return;
        }
        let cancel = {
            let mut job = self.stem_job.lock().unwrap();
            if let Some((running, flag)) = job.as_ref() {
                if running == key {
                    return; // already on it
                }
                flag.store(true, Ordering::Relaxed);
            }
            let flag = Arc::new(AtomicBool::new(false));
            *job = Some((key.to_string(), flag.clone()));
            flag
        };
        *self.stem_state.lock().unwrap() = (key.to_string(), StemState::Loading);
        self.stem_progress.store(0.0, Ordering::Relaxed);

        let shared = self.clone();
        let (key, path) = (key.to_string(), path.to_path_buf());
        std::thread::Builder::new()
            .name("tempo-sampler-stems".into())
            .spawn(move || {
                let set_state = |state: StemState| {
                    if !cancel.load(Ordering::Relaxed) {
                        *shared.stem_state.lock().unwrap() = (key.clone(), state);
                    }
                };
                let result = (|| -> Result<Option<stems::Stems>, String> {
                    let dir = stems::cache_dir_for(&path);
                    if let Some(cached) = dir.as_deref().and_then(stems::load_cached) {
                        return Ok(Some(cached));
                    }
                    set_state(StemState::Splitting);
                    // Usually already decoded (or being decoded) for playback.
                    let src = shared.decoded(&key, Some(&path), "the song")?;
                    let separated = stems::with_separator(|sep| {
                        sep.separate(
                            &src.samples,
                            src.sample_rate,
                            |p| shared.stem_progress.store(p, Ordering::Relaxed),
                            || cancel.load(Ordering::Relaxed),
                        )
                    })??;
                    if let (Some(st), Some(dir)) = (&separated, &dir) {
                        // A failed cache write only costs a re-split later.
                        let _ = stems::save_cached(dir, st);
                    }
                    Ok(separated)
                })();
                match result {
                    Ok(Some(st)) if !cancel.load(Ordering::Relaxed) => {
                        *shared.stems.lock().unwrap() = Some((key.clone(), Arc::new(st)));
                        set_state(StemState::Ready);
                        // Apply the switches if this song is still selected.
                        let switches = shared.stem_switches.load(Ordering::Relaxed);
                        let rerender = {
                            let mut sel = shared.selection.lock().unwrap();
                            if sel.key == key && sel.stem_mask != switches {
                                sel.stem_mask = switches;
                                true
                            } else {
                                false
                            }
                        };
                        if rerender {
                            shared.render_now();
                        }
                    }
                    Ok(_) => {} // cancelled
                    Err(e) => set_state(StemState::Failed(e)),
                }
                let mut job = shared.stem_job.lock().unwrap();
                if job.as_ref().is_some_and(|(k, _)| *k == key) {
                    *job = None;
                }
            })
            .ok();
    }

    /// Waveform overviews of every chop, mixed from the enabled stems.
    /// Cached per song and mask; `None` if the stems aren't ready.
    pub fn mixed_chop_overviews(&self, mask: u8) -> Option<Arc<Vec<Vec<(f32, f32)>>>> {
        let view = self.song_view.lock().unwrap().clone()?;
        if let Some((key, m, cached)) = self.mixed_overviews.lock().unwrap().as_ref() {
            if *key == view.key && *m == mask {
                return Some(cached.clone());
            }
        }
        let stems = self.stems_for(&view.key)?;
        let overviews: Vec<Vec<(f32, f32)>> = view
            .chops
            .iter()
            .map(|c| min_max_overview_mixed(&stems, mask, c.start, c.end, CHOP_OVERVIEW_POINTS))
            .collect();
        let overviews = Arc::new(overviews);
        *self.mixed_overviews.lock().unwrap() = Some((view.key.clone(), mask, overviews.clone()));
        Some(overviews)
    }

    /// Exports the current selection at the host tempo (the drag area and
    /// "Show WAV in Finder").
    pub fn export_current(&self) -> Result<PathBuf, String> {
        let selection = self.selection.lock().unwrap().clone();
        self.export_for_drag(&selection, self.transport.tempo.load(Ordering::Relaxed))
    }

    /// Writes `selection` stretched to `bpm` as a WAV and returns its path,
    /// for dragging into the host. Uses the current render when it matches;
    /// otherwise stretches right here, so a drag can start the moment the
    /// mouse moves. That's only done for short audio (chops and loops take
    /// a few hundredths of a second); songs must already be rendered.
    pub fn export_for_drag(&self, selection: &Selection, bpm: f32) -> Result<PathBuf, String> {
        if bpm <= 0.0 {
            return Err("No tempo from Ableton yet".into());
        }
        let wanted = selection.render_key();
        if let Some(r) = self.current() {
            if r.key == wanted && (r.bpm - bpm).abs() < 1e-3 && !r.partial {
                return self.write_export(r);
            }
        }
        let src: Arc<Source> = match (&selection.chop, &selection.path) {
            // A chop: its song is already decoded. Never wait on a decode
            // in progress (that would freeze the window).
            (Some(_), _) => self
                .source
                .try_lock()
                .ok()
                .and_then(|g| g.as_ref().filter(|(k, _)| *k == selection.key).map(|(_, s)| s.clone()))
                .ok_or("Still loading the song, try again in a moment")?,
            (None, None) => {
                let a = LoopAsset::test_loop();
                Arc::new(Source { samples: a.samples, sample_rate: a.sample_rate })
            }
            // Already decoded for playback (don't wait if it's mid-decode).
            (None, Some(_))
                if self
                    .source
                    .try_lock()
                    .is_ok_and(|g| g.as_ref().is_some_and(|(k, _)| *k == selection.key)) =>
            {
                let src = self.source.try_lock().ok().and_then(|g| g.as_ref().map(|(_, s)| s.clone()));
                let src = src.ok_or("Still loading, try again in a moment")?;
                let secs = src.frames() as f32 / src.sample_rate as f32;
                if secs > bpm::LOOP_MAX_SECS {
                    return Err("Still stretching the song, try again in a moment".into());
                }
                src
            }
            (None, Some(path)) => {
                let (secs, _) = audio_file::probe(path)?;
                if secs > bpm::LOOP_MAX_SECS {
                    return Err("Still stretching the song, try again in a moment".into());
                }
                let (samples, sample_rate) = audio_file::decode(path)?;
                Arc::new(Source { samples: Arc::new(samples), sample_rate })
            }
        };
        let asset = self.make_asset(selection, &src)?;
        self.write_export(Arc::new(asset.stretch_to(bpm)))
    }

    /// Writes a render to the export folder. Song renders can be tens of
    /// MB, so the same render is written at most once.
    fn write_export(&self, rendered: Arc<RenderedLoop>) -> Result<PathBuf, String> {
        let mut exported = self.exported.lock().unwrap();
        if let Some((done, path)) = exported.as_ref() {
            if Arc::ptr_eq(done, &rendered) && path.exists() {
                return Ok(path.clone());
            }
        }
        let path = rendered.write_wav(&loops::export_dir()).map_err(|e| e.to_string())?;
        *exported = Some((rendered, path.clone()));
        Ok(path)
    }
}

pub enum Task {
    Render { id: u64, bpm: f32 },
}

pub struct TempoSampler {
    params: Arc<TempoSamplerParams>,
    shared: Arc<Shared>,

    // Audio-thread-only state.
    from_render: rtrb::Consumer<Arc<RenderedLoop>>,
    to_garbage: rtrb::Producer<Arc<RenderedLoop>>,
    playing_loop: Option<Arc<RenderedLoop>>,
    sample_rate: f32,
    requested_bpm: f32,
    pending_bpm: f32,
    pending_samples: usize,
    /// Song position (in beats) used while the transport is stopped, so a
    /// preview started while stopped keeps looping on its own.
    free_beats: f64,
    /// Transport state last block, to detect Ableton's stop button.
    was_playing: bool,
    /// Songs (non-loops) play from the start rather than locked to the bar
    /// grid; this is the play position in frames of the rendered buffer.
    song_pos: f64,
    /// Preview state last block, to restart songs when the preview starts.
    was_preview_on: bool,
    /// Smoothed preview on/off level, 0.0..=1.0.
    preview_level: f32,
    preview_fade_coef: f32,
}

#[derive(Params)]
pub struct TempoSamplerParams {
    /// Window size, saved with the Ableton set.
    #[persist = "editor-state"]
    editor_state: Arc<ViziaState>,
}

impl Default for TempoSampler {
    fn default() -> Self {
        let (to_audio, from_render) = rtrb::RingBuffer::new(8);
        let (to_garbage, garbage) = rtrb::RingBuffer::new(16);
        let shared = Shared {
            transport: HostTransport::default(),
            library: Arc::new(Library::default()),
            selection: Mutex::new(Selection::builtin()),
            source: Mutex::new(None),
            loaded: Mutex::new(None),
            song_view: Mutex::new(None),
            song_view_version: AtomicU64::new(0),
            analysing: Mutex::new(None),
            playhead: AtomicF32::new(-1.0),
            chop_length: AtomicUsize::new(2),
            stem_switches: AtomicU8::new(ALL_STEMS),
            stems: Mutex::new(None),
            mixed_overviews: Mutex::new(None),
            stem_state: Mutex::new((String::new(), StemState::Idle)),
            stem_progress: AtomicF32::new(0.0),
            stem_job: Mutex::new(None),
            publish_lock: Mutex::new(()),
            latest_request: AtomicU64::new(0),
            rendered_request: AtomicU64::new(0),
            rendered_bpm: AtomicF32::new(0.0),
            render_error: Mutex::new(None),
            preview: AtomicBool::new(false),
            current: Mutex::new(None),
            current_overview: Mutex::new(Arc::new(Vec::new())),
            exported: Mutex::new(None),
            to_audio: Mutex::new(to_audio),
            garbage: Mutex::new(garbage),
        };
        Self {
            params: Arc::new(TempoSamplerParams {
                editor_state: editor::default_state(),
            }),
            shared: Arc::new(shared),
            from_render,
            to_garbage,
            playing_loop: None,
            sample_rate: 48_000.0,
            requested_bpm: 0.0,
            pending_bpm: 0.0,
            pending_samples: 0,
            free_beats: 0.0,
            was_playing: false,
            song_pos: 0.0,
            was_preview_on: false,
            preview_level: 0.0,
            preview_fade_coef: 1.0,
        }
    }
}

impl Plugin for TempoSampler {
    const NAME: &'static str = "Tempo Sampler";
    const VENDOR: &'static str = "Tempo Sampler";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    // Stereo in, stereo out. Input passes through; the preview is mixed on top.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(2),
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];

    type SysExMessage = ();
    type BackgroundTask = Task;

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn task_executor(&mut self) -> TaskExecutor<Self> {
        let shared = self.shared.clone();
        Box::new(move |task| match task {
            Task::Render { id, bpm } => shared.render(id, bpm),
        })
    }

    fn editor(&mut self, async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(
            self.shared.clone(),
            async_executor,
            self.params.editor_state.clone(),
        )
    }

    fn initialize(
        &mut self,
        _layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate = buffer_config.sample_rate;
        self.preview_fade_coef = 1.0 - (-1.0 / (PREVIEW_FADE_SECS * self.sample_rate)).exp();
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let t = context.transport();
        let shared = &self.shared;
        let out = &shared.transport;

        // 1. Publish transport info for the GUI. Ableton reports tempo even while stopped.
        if let Some(tempo) = t.tempo {
            out.tempo.store(tempo as f32, Ordering::Relaxed);
        }
        out.playing.store(t.playing, Ordering::Relaxed);
        if let Some(pos) = t.pos_beats() {
            out.pos_beats.store(pos, Ordering::Relaxed);
        }
        if let (Some(num), Some(den)) = (t.time_sig_numerator, t.time_sig_denominator) {
            out.time_sig_numerator.store(num, Ordering::Relaxed);
            out.time_sig_denominator.store(den, Ordering::Relaxed);
        }

        // 2. Ask for a re-stretch once the tempo has been stable for a moment.
        if let Some(tempo) = t.tempo {
            let tempo = tempo as f32;
            if tempo != self.requested_bpm {
                if tempo != self.pending_bpm {
                    self.pending_bpm = tempo;
                    self.pending_samples = 0;
                }
                self.pending_samples += buffer.samples();
                if self.pending_samples as f32 >= RENDER_DEBOUNCE_SECS * self.sample_rate {
                    self.requested_bpm = tempo;
                    let id = shared.next_request();
                    context.execute_background(Task::Render { id, bpm: tempo });
                }
            }
        }

        // 3. Pick up finished renders (keep only the newest).
        while let Ok(new_loop) = self.from_render.pop() {
            // Same song re-stretched (a new tempo, or the full render taking
            // over from its start): keep our place in it, scaled by tempo.
            // A different file: start from the top.
            self.song_pos = match self.playing_loop.as_deref() {
                Some(old) if old.key == new_loop.key && new_loop.bpm > 0.0 => {
                    self.song_pos * old.bpm as f64 / new_loop.bpm as f64
                }
                _ => 0.0,
            };
            if let Some(old) = self.playing_loop.replace(new_loop) {
                // Hand the old buffer back so it's freed off the audio thread.
                // If that queue is somehow full, dropping here is the fallback.
                let _ = self.to_garbage.push(old);
            }
        }

        // 4. Mix the preview, locked to the host's beat position.
        let host_tempo = t.tempo.unwrap_or(120.0);
        let beats_per_sample = host_tempo / 60.0 / self.sample_rate as f64;
        let start_beats = if t.playing {
            t.pos_beats().unwrap_or(self.free_beats)
        } else {
            self.free_beats
        };
        // Ableton's stop button stops the preview too. Starting the preview
        // while stopped still works (it free-runs) for auditioning.
        if self.was_playing && !t.playing {
            shared.preview.store(false, Ordering::Relaxed);
        }
        self.was_playing = t.playing;
        let preview_on = shared.preview.load(Ordering::Relaxed);
        if preview_on && !self.was_preview_on {
            self.song_pos = 0.0; // each audition of a song starts at the top
        }
        self.was_preview_on = preview_on;
        let num_samples = buffer.samples();

        for (i, mut frame) in buffer.iter_samples().enumerate() {
            let target = if preview_on { 1.0 } else { 0.0 };
            self.preview_level += (target - self.preview_level) * self.preview_fade_coef;

            let Some(lp) = self.playing_loop.as_deref() else { continue };
            if self.preview_level < 1e-5 {
                continue;
            }
            let frames = lp.frames();
            if frames == 0 {
                continue;
            }
            let pos = if lp.is_loop {
                // Where are we inside the loop? Map beats -> fraction of loop
                // -> frame. Being beat-based, it stays in sync even
                // mid-tempo-change and handles sample-rate differences for free.
                let beats = start_beats + i as f64 * beats_per_sample;
                let phase = (beats / lp.beats as f64).rem_euclid(1.0);
                phase * frames as f64
            } else {
                // Songs: play through from the start (looping at the end).
                let pos = self.song_pos;
                let step = lp.sample_rate as f64 / self.sample_rate as f64;
                self.song_pos = (pos + step) % frames as f64;
                pos
            };
            let i0 = (pos as usize).min(frames - 1);
            let i1 = (i0 + 1) % frames;
            let frac = (pos - i0 as f64) as f32;
            // Unity gain: level is set on the track the clip lands on.
            let level = self.preview_level;
            for ch in 0..2 {
                let s = lp.samples[i0 * 2 + ch] * (1.0 - frac) + lp.samples[i1 * 2 + ch] * frac;
                if let Some(o) = frame.get_mut(ch) {
                    *o += s * level;
                }
            }
        }
        self.free_beats = start_beats + num_samples as f64 * beats_per_sample;

        // 5. Report where the preview is within the selected song.
        let playhead = match self.playing_loop.as_deref() {
            Some(lp) if self.preview_level > 1e-3 && lp.frames() > 0 => match lp.span {
                Some((a, b)) => {
                    let frac = if lp.is_loop {
                        let beats = start_beats + num_samples as f64 * beats_per_sample;
                        (beats / lp.beats as f64).rem_euclid(1.0) as f32
                    } else {
                        (self.song_pos / lp.frames() as f64) as f32
                    };
                    a + frac * (b - a)
                }
                None => -1.0,
            },
            _ => -1.0,
        };
        shared.playhead.store(playhead, Ordering::Relaxed);

        ProcessStatus::Normal
    }
}

impl ClapPlugin for TempoSampler {
    const CLAP_ID: &'static str = "dev.temposampler.tempo-sampler";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Shows the host tempo and stretches a loop to match it");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Stereo,
        ClapFeature::Utility,
    ];
}

impl Vst3Plugin for TempoSampler {
    // Must be exactly 16 bytes and never change once people use the plugin,
    // or Ableton will treat it as a different plugin.
    const VST3_CLASS_ID: [u8; 16] = *b"TempoSamplerV001";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Tools];
}

nih_export_clap!(TempoSampler);
nih_export_vst3!(TempoSampler);

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_rendered(shared: &Shared, id: u64) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while shared.rendered_request.load(Ordering::Relaxed) != id {
            assert!(Instant::now() < deadline, "render {id} did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn song_select_analyse_and_render_chop() {
        // A 40 s "song": the test loop at 97 BPM repeated, after a short intro.
        let looped = LoopAsset::test_loop().stretch_to(97.0).samples;
        let mut samples = vec![0.0f32; 2 * 24_000];
        while samples.len() < 2 * 48_000 * 40 {
            samples.extend_from_slice(&looped);
        }
        let dir = std::env::temp_dir().join(format!("tempo-sampler-engine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        samples.iter().for_each(|s| w.write_sample(*s).unwrap());
        w.finalize().unwrap();

        let plugin = TempoSampler::default();
        let shared = plugin.shared.clone();
        *shared.selection.lock().unwrap() = Selection {
            key: path.to_string_lossy().into(),
            path: Some(path.clone()),
            name: "song".into(),
            bpm: 97.0,
            chop: None,
            stem_mask: ALL_STEMS,
        };

        // Whole song at 120 BPM: analysed, then stretched on its own thread.
        let id = shared.next_request();
        shared.render(id, 120.0);
        let deadline = Instant::now() + Duration::from_secs(30);
        while shared.song_view.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "song not analysed");
            std::thread::sleep(Duration::from_millis(5));
        }
        let view = shared.song_view.lock().unwrap().clone().expect("song analysed");
        assert!(view.chops.len() >= chops::MIN_CHOPS);
        assert_eq!(view.chop_overviews.len(), view.chops.len());
        wait_rendered(&shared, id);
        let whole = shared.current().unwrap();
        assert!(!whole.is_loop);
        assert_eq!(whole.span, Some((0.0, 1.0)));
        let expected = (samples.len() / 2) as f64 * 97.0 / 120.0;
        assert!((whole.frames() as f64 - expected).abs() < 2.0);

        // Pick the first chop: rendered as an exact loop at 120 BPM.
        let chop = view.chops[0].clone();
        shared.selection.lock().unwrap().chop = Some(ChopSel { index: 0, number: 1, chop: chop.clone() });
        let id = shared.next_request();
        shared.render(id, 120.0);
        wait_rendered(&shared, id);
        let r = shared.current().unwrap();
        assert!(r.is_loop);
        assert_eq!(r.key, shared.selection.lock().unwrap().render_key());
        assert_eq!(r.name, format!("song - {} bar chop 1", chop.bars));
        let bar_frames = 4.0 * 60.0 / 120.0 * 48_000.0;
        let bars = r.frames() as f32 / bar_frames;
        assert!((bars - chop.bars as f32).abs() < 0.001, "{bars} bars, want {}", chop.bars);
        let (a, b) = r.span.unwrap();
        assert!(a < b && b <= 1.0);

        // Dragging a chop that hasn't been rendered stretches it on the spot.
        std::env::set_var("TEMPO_SAMPLER_EXPORT_DIR", dir.join("export"));
        let other = view.chops.iter().position(|c| c.bars != chop.bars || c.start != chop.start).unwrap();
        let mut sel = shared.selection.lock().unwrap().clone();
        sel.chop = Some(ChopSel { index: other, number: 2, chop: view.chops[other].clone() });
        let t = Instant::now();
        let path = shared.export_for_drag(&sel, 120.0).unwrap();
        assert!(t.elapsed() < Duration::from_millis(500), "took {:?}", t.elapsed());
        let reader = hound::WavReader::open(&path).unwrap();
        let bars = reader.duration() as f32 / bar_frames;
        assert!((bars - view.chops[other].bars as f32).abs() < 0.001, "{bars} bars");
        assert!(path.starts_with(dir.join("export")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stereo_overview_matches_mono_overview() {
        let x: Vec<f32> = (0..20_001).map(|i| ((i as f32) * 0.37).sin() * (i % 7) as f32 * 0.1).collect();
        let x = &x[..20_000];
        let mono: Vec<f32> = x.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
        assert_eq!(min_max_overview_stereo(x, 37), min_max_overview(&mono, 37));
        let st = stems::Stems { sample_rate: 48_000, stems: [x.to_vec(), x.to_vec(), vec![0.0; x.len()], x.to_vec()] };
        let mixed = stems::mix(&st, 0b1011, 100, 9_000);
        let mono: Vec<f32> = mixed.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
        assert_eq!(min_max_overview_mixed(&st, 0b1011, 100, 9_000, 37), min_max_overview(&mono, 37));
    }

    /// Times every main path on real files. Not part of the normal run:
    ///   TEMPO_SAMPLER_BENCH_FILES="loop.aif;song.wav;..." \
    ///   cargo test --release --lib bench_paths -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_paths() {
        let ms = |t: Instant| format!("{:7.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        let files = std::env::var("TEMPO_SAMPLER_BENCH_FILES").expect("TEMPO_SAMPLER_BENCH_FILES");
        let export = std::env::temp_dir().join(format!("tempo-sampler-bench-{}", std::process::id()));
        std::env::set_var("TEMPO_SAMPLER_EXPORT_DIR", &export);

        let t = Instant::now();
        let plugin = TempoSampler::default();
        eprintln!("plugin instance               {}", ms(t));
        let shared = plugin.shared.clone();
        shared.transport.tempo.store(120.0, Ordering::Relaxed);

        for file in files.split(';').filter(|f| !f.is_empty()) {
            let path = std::path::PathBuf::from(file);
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            eprintln!("\n== {name}");
            let t = Instant::now();
            let (stereo, sr) = audio_file::decode(&path).unwrap();
            eprintln!("decode                        {}  ({:.1} s at {sr} Hz)", ms(t), stereo.len() as f32 / 2.0 / sr as f32);
            let mono: Vec<f32> = stereo.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
            let t = Instant::now();
            let bpm = bpm::detect_bpm(&mono, sr).unwrap_or(120.0);
            eprintln!("detect tempo                  {}  ({bpm:.2} BPM)", ms(t));
            let t = Instant::now();
            let (window, wsr, wsecs) = audio_file::decode_mono_window(&path).unwrap();
            let wbpm = bpm::detect_bpm_window(&window, wsr, wsecs).unwrap_or(120.0);
            eprintln!("library tempo (window decode) {}  ({wbpm:.2} BPM)", ms(t));
            let (s0, l0) = bpm::analysis_window(mono.len(), sr);
            eprintln!("  window same as full decode: {}", window == mono[s0..s0 + l0]);
            let secs = stereo.len() as f32 / 2.0 / sr as f32;
            let is_song = !bpm::is_loop(secs, bpm) && secs >= SONG_MIN_SECS;
            if is_song {
                let t = Instant::now();
                let a = chops::analyse_song(&mono, sr, bpm);
                eprintln!("analyse song (chops)          {}  ({} chops)", ms(t), a.chops.len());
            }
            drop((stereo, mono));

            *shared.source.lock().unwrap() = None;
            *shared.song_view.lock().unwrap() = None;
            *shared.selection.lock().unwrap() = Selection {
                key: path.to_string_lossy().into(),
                path: Some(path.clone()),
                name: name.clone(),
                bpm,
                chop: None,
                stem_mask: ALL_STEMS,
            };
            let t = Instant::now();
            let id = shared.next_request();
            shared.render(id, 120.0);
            eprintln!("select: render call returns   {}", ms(t));
            let want = shared.selection.lock().unwrap().render_key();
            while !shared.current().is_some_and(|r| r.key == want && r.bpm == 120.0) {
                std::thread::sleep(Duration::from_millis(1));
            }
            eprintln!("select: first sound           {}", ms(t));
            wait_rendered(&shared, id);
            eprintln!("select: full render ready     {}", ms(t));
            let t = Instant::now();
            let id = shared.next_request();
            shared.render(id, 121.0);
            wait_rendered(&shared, id);
            eprintln!("re-render (tempo change)      {}", ms(t));
            let sel = shared.selection.lock().unwrap().clone();
            let t = Instant::now();
            let exported = shared.export_for_drag(&sel, 121.0);
            eprintln!("drag whole file (export)      {}  ({})", ms(t), if exported.is_ok() { "ok" } else { "failed" });

            if is_song {
                while shared.song_view.lock().unwrap().is_none() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let view = shared.song_view.lock().unwrap().clone().unwrap();
                for (n, i) in [(1, 0usize), (2, 1)] {
                    let chop = view.chops[i].clone();
                    shared.selection.lock().unwrap().chop = Some(ChopSel { index: i, number: n, chop });
                    let t = Instant::now();
                    let id = shared.next_request();
                    shared.render(id, 120.0);
                    wait_rendered(&shared, id);
                    eprintln!("click chop {n}: until audible   {}", ms(t));
                }
                let t = Instant::now();
                shared.export_current().unwrap();
                eprintln!("drag current chop (export)    {}", ms(t));
                let mut sel = shared.selection.lock().unwrap().clone();
                sel.chop = Some(ChopSel { index: 5, number: 6, chop: view.chops[5].clone() });
                let t = Instant::now();
                shared.export_for_drag(&sel, 120.0).unwrap();
                eprintln!("drag unrendered chop          {}", ms(t));
                if let Some(dir) = stems::cache_dir_for(&path) {
                    let t = Instant::now();
                    if let Some(st) = stems::load_cached(&dir) {
                        eprintln!("load cached stems             {}", ms(t));
                        let t = Instant::now();
                        let mixed = stems::mix(&st, 0b0101, 0, st.stems[0].len() / 2);
                        eprintln!("mix 2 of 4 stems (whole song) {}  ({} samples)", ms(t), mixed.len());
                        *shared.stems.lock().unwrap() = Some((sel.key.clone(), Arc::new(st)));
                        let t = Instant::now();
                        let o = shared.mixed_chop_overviews(0b0101);
                        eprintln!("mixed chop overviews          {}  ({:?})", ms(t), o.map(|o| o.len()));
                    } else {
                        eprintln!("(no cached stems)");
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&export);
    }

    /// Needs models/htdemucs_fp16weights.onnx (not in git); skipped otherwise.
    #[test]
    fn stems_split_cache_and_mask_a_chop() {
        let model = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("models/htdemucs_fp16weights.onnx");
        if !model.exists() {
            eprintln!("skipping: model not present");
            return;
        }
        let dir = std::env::temp_dir().join(format!("tempo-sampler-stemflow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("TEMPO_SAMPLER_MODEL", &model);
        std::env::set_var("TEMPO_SAMPLER_STEM_CACHE", dir.join("cache"));

        // 24 s "song": the drum + pluck loop at 97 BPM.
        let looped = LoopAsset::test_loop().stretch_to(97.0).samples;
        let mut samples = Vec::new();
        while samples.len() < 2 * 48_000 * 24 {
            samples.extend_from_slice(&looped);
        }
        // Not a whole number of bars, so it's treated as a song, not a loop.
        samples.truncate(2 * 48_000 * 24);
        let path = dir.join("song.wav");
        let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        samples.iter().for_each(|s| w.write_sample(*s).unwrap());
        w.finalize().unwrap();

        let plugin = TempoSampler::default();
        let shared = plugin.shared.clone();
        shared.transport.tempo.store(120.0, Ordering::Relaxed);
        let key = path.to_string_lossy().to_string();
        *shared.selection.lock().unwrap() = Selection {
            key: key.clone(),
            path: Some(path.clone()),
            name: "song".into(),
            bpm: 97.0,
            chop: None,
            stem_mask: ALL_STEMS,
        };
        let id = shared.next_request();
        shared.render(id, 120.0);
        shared.request_stems(&key, &path);
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let state = shared.stem_state.lock().unwrap().1.clone();
            match state {
                StemState::Ready => break,
                StemState::Failed(e) => panic!("split failed: {e}"),
                _ => {}
            }
            assert!(Instant::now() < deadline, "split did not finish");
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(stems::cache_dir_for(&path).unwrap().join("drums.wav").exists(), "stems cached");

        // Drums only, on the first chop.
        let view = shared.song_view.lock().unwrap().clone().unwrap();
        let chop = view.chops[0].clone();
        shared.stem_switches.store(0b0001, Ordering::Relaxed);
        {
            let mut sel = shared.selection.lock().unwrap();
            sel.chop = Some(ChopSel { index: 0, number: 1, chop: chop.clone() });
            sel.stem_mask = shared.effective_stem_mask(&key);
            assert_eq!(sel.stem_mask, 0b0001);
        }
        let id = shared.next_request();
        shared.render(id, 120.0);
        wait_rendered(&shared, id);
        let drums = shared.current().unwrap();
        assert!(drums.key.ends_with("|stems0001"), "{}", drums.key);
        assert!(drums.name.ends_with("(drums)"), "{}", drums.name);

        // Same chop, all stems: louder than drums alone (the pluck is back).
        shared.selection.lock().unwrap().stem_mask = ALL_STEMS;
        let id = shared.next_request();
        shared.render(id, 120.0);
        wait_rendered(&shared, id);
        let full = shared.current().unwrap();
        let energy = |v: &[f32]| v.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
        assert_eq!(full.frames(), drums.frames());
        assert!(energy(&full.samples) > energy(&drums.samples) * 1.05);

        // Switching stems never touched the source file.
        assert_eq!(hound::WavReader::open(&path).unwrap().duration() as usize, samples.len() / 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
