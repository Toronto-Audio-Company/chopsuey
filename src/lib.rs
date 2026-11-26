use crate::safety::LockExt;
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
pub mod key;
pub mod library;
#[macro_use]
pub mod log;
mod editor;
mod logo;
pub mod loops;
pub mod one_shots;
pub mod safety;
pub mod stems;

use library::Library;
use loops::{LoopAsset, Pitch, RenderedLoop};

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
    /// Bar-aligned chops, then the one-shots (`bars == one_shots::ONE_SHOT`).
    pub chops: Vec<chops::Chop>,
    /// (min, max) waveform of each chop in `CHOP_OVERVIEW_POINTS` slices.
    pub chop_overviews: Vec<Vec<(f32, f32)>>,
    /// Stem mask of the audio the one-shots were found in: they follow the
    /// stem switches (solo the horns and their blasts are one-shots).
    pub one_shot_mask: u8,
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
    /// `loops::key_hash` of the clip the GUI last asked for (0: any). The
    /// audio thread plays a render only if it's that clip.
    pub wanted_key: AtomicU64,
    /// What the current / in-progress pad set was built for (see
    /// `refresh_pads`), and the queue that hands sets to the audio thread.
    pads_sig: Mutex<String>,
    to_audio_pads: Mutex<rtrb::Producer<Arc<PadSet>>>,
    pad_garbage: Mutex<rtrb::Consumer<Arc<PadSet>>>,
    /// Bit i set: the chop tile with `song_view.chops` index i has its pad
    /// held (tiles past 63 don't light up).
    pub pads_down: AtomicU64,
    /// The audio thread hit a bug and went silent (see `process`). The GUI
    /// says so; re-initialising the plugin clears it.
    pub audio_fault: AtomicBool,
    /// Saved with the Live set (the same Arc as the persisted param field).
    pub session: Arc<Mutex<SessionState>>,
    /// The session state currently in effect (last restored or saved). When
    /// `session` differs, the host has loaded new state that needs applying.
    applied_session: Mutex<SessionState>,
    /// A chop to re-select once the restored song has been analysed.
    restore_chop: Mutex<Option<(String, usize, usize)>>,
    /// Gain that plays the selected sample at a matched loudness (1.0 if
    /// unknown). Previews only; exports are untouched.
    pub preview_gain: AtomicF32,
    /// The host's sample rate (0 until known); renders are converted to it.
    pub host_rate: std::sync::atomic::AtomicU32,
    /// Transpose (semitones) and Detune (cents) applied to every render.
    transpose: AtomicI32,
    detune: AtomicI32,
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
    /// Makes sure the audio thread has pads for what's selected: the chop
    /// tiles of the shown length at the host tempo (rendered here, on a
    /// background thread, in parallel), or "play the current clip" for a
    /// loop or short sample. Cheap when nothing changed.
    pub fn refresh_pads(self: &Arc<Self>) {
        let tempo = self.transport.tempo.load(Ordering::Relaxed);
        let selection = self.selection.locked().clone();
        let length = self.chop_length.load(Ordering::Relaxed);
        let view = self.song_view.locked().clone().filter(|v| v.key == selection.key);
        let sig = match &view {
            Some(v) if tempo > 0.0 => format!(
                "{}|{}|{}|{length}|{tempo}|{}|{}|{:?}",
                v.key,
                v.bpm,
                self.song_view_version.load(Ordering::Relaxed),
                selection.stem_mask,
                self.host_rate.load(Ordering::Relaxed),
                self.pitch()
            ),
            _ => "current".to_string(),
        };
        {
            let mut built = self.pads_sig.locked();
            if *built == sig {
                return;
            }
            *built = sig.clone();
        }
        let send = |set: PadSet| {
            while self.pad_garbage.locked().pop().is_ok() {}
            let _ = self.to_audio_pads.locked().push(Arc::new(set));
        };
        let Some(view) = view.filter(|_| tempo > 0.0) else {
            send(PadSet { clips: Vec::new(), chop_indices: Vec::new(), play_current: true, gain: 1.0 });
            return;
        };
        // Silence the old song's pads at once, then render this one's.
        send(PadSet { clips: Vec::new(), chop_indices: Vec::new(), play_current: false, gain: 1.0 });
        let shared = self.clone();
        crate::safety::spawn("tempo-sampler-pads", move || {
            let Ok(src) = shared.source(&selection) else { return };
            let indices: Vec<usize> = (0..view.chops.len()).filter(|&i| view.chops[i].bars == length).collect();
            let total = view.frames.max(1) as f32;
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
            let rendered = library::parallel_map(&indices, threads, |&i| {
                let chop = view.chops[i].clone();
                let number = indices.iter().position(|&j| j == i).unwrap_or(0) + 1;
                let mut sel = selection.clone();
                sel.chop = Some(ChopSel { index: i, number, chop: chop.clone() });
                let asset = shared.make_asset(&sel, &src).ok()?;
                let mut r = shared.at_host_rate(asset.stretch_to(tempo));
                r.span = Some((chop.start as f32 / total, chop.end as f32 / total));
                Some((i, Arc::new(r)))
            });
            if *shared.pads_sig.locked() != sig {
                return; // superseded while rendering
            }
            let (chop_indices, clips) = rendered.into_iter().unzip();
            while shared.pad_garbage.locked().pop().is_ok() {}
            let _ = shared
                .to_audio_pads
                
                .locked()
                .push(Arc::new(PadSet {
                    clips,
                    chop_indices,
                    play_current: false,
                    gain: shared.loudness_gain(&selection.key),
                }));
        });
    }

    /// Brings the plugin in line with its parameters, the way the same
    /// clicks in the GUI would. Does nothing for values already in effect,
    /// so the GUI (which sets both) and the host can't fight.
    pub fn apply_params(self: &Arc<Self>, values: ParamValues) {
        let ParamValues { stem_mask, chop_length, tempo_scale, pitch } = values;
        // Transpose / Detune: everything is re-rendered at the new pitch.
        let mut rerender = self.set_pitch(pitch);
        // Stems (at least one stays on). Applied to the selection once its
        // stems are ready; until then the switches wait.
        if stem_mask != 0 && self.stem_switches.swap(stem_mask, Ordering::Relaxed) != stem_mask {
            let mut sel = self.selection.locked();
            if self.stems_for(&sel.key).is_some() && sel.stem_mask != stem_mask {
                sel.stem_mask = stem_mask;
                rerender = true;
            }
        }
        if self.chop_length.swap(chop_length, Ordering::Relaxed) != chop_length {
            self.refresh_pads();
            self.save_session();
        }
        // ½× / 1× / 2× for the selected file (saved per file, as in the GUI).
        let key = self.selection.locked().key.clone();
        if let Some(entry) = self.library.entry(&key) {
            if let (Some(base), true) = (entry.base_bpm, entry.scale != tempo_scale) {
                self.library.set_scale(&key, tempo_scale);
                let mut sel = self.selection.locked();
                if sel.key == key {
                    sel.bpm = base * tempo_scale;
                    sel.chop = None; // chops belong to the old tempo's analysis
                    rerender = true;
                }
            }
        }
        if rerender {
            self.render_now();
        }
    }

    /// The pitch shift applied to renders.
    pub fn pitch(&self) -> Pitch {
        Pitch { semitones: self.transpose.load(Ordering::Relaxed), cents: self.detune.load(Ordering::Relaxed) }
    }

    /// Sets the pitch shift; true if it changed.
    fn set_pitch(&self, pitch: Pitch) -> bool {
        let old = self.pitch();
        self.transpose.store(pitch.semitones, Ordering::Relaxed);
        self.detune.store(pitch.cents, Ordering::Relaxed);
        old != pitch
    }

    /// The gain that plays `key` at the common preview loudness.
    fn loudness_gain(&self, key: &str) -> f32 {
        self.library.entry(key).and_then(|e| e.loudness).map_or(1.0, key::matching_gain)
    }

    /// Converts a render to the host's sample rate (high-quality windowed
    /// sinc), so the audio thread plays it sample for sample instead of
    /// interpolating, which would alias.
    fn at_host_rate(&self, mut r: RenderedLoop) -> RenderedLoop {
        let host = self.host_rate.load(Ordering::Relaxed);
        if host > 0 && host != r.sample_rate {
            r.samples = dsp::resample_stereo(&r.samples, r.sample_rate, host);
            r.sample_rate = host;
        }
        r
    }

    /// Marks the current selection as the clip the audio thread should play
    /// (not from the audio thread: it locks the selection).
    pub fn want_selection(&self) {
        let key = self.selection.locked().render_key();
        self.wanted_key.store(loops::key_hash(&key), Ordering::Relaxed);
        self.save_session();
    }

    /// Copies what's selected into the state saved with the Live set.
    pub fn save_session(&self) {
        let sel = self.selection.locked().clone();
        let file_size = sel.path.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len());
        let analysed = self.song_view.locked().as_ref().is_some_and(|v| v.key == sel.key);
        let mut session = self.session.locked();
        let is_song = analysed || (session.is_song && session.key == sel.key);
        let mut applied = self.applied_session.locked();
        *session = SessionState {
            is_song,
            chop: sel.chop.as_ref().map(|c| (c.chop.start, c.chop.end)),
            key: sel.key,
            path: sel.path,
            name: sel.name,
            bpm: sel.bpm,
            chop_length: self.chop_length.load(Ordering::Relaxed),
            stem_switches: self.stem_switches.load(Ordering::Relaxed),
            version: SESSION_VERSION,
            file_size,
        };
        *applied = session.clone();
    }

    /// Restores the session if the host loaded state that isn't in effect
    /// yet. True if it did (the selection changed, so re-render).
    fn restore_if_loaded(self: &Arc<Self>) -> bool {
        let saved = self.session.locked().clone();
        if *self.applied_session.locked() == saved {
            return false;
        }
        *self.applied_session.locked() = saved;
        self.restore_session()
    }

    /// Puts back what the Live set saved (called when the plugin starts;
    /// the audio thread then renders it once it knows the tempo, and the
    /// chop comes back after the song is analysed).
    fn restore_session(self: &Arc<Self>) -> bool {
        let mut saved = self.session.locked().clone();
        if saved.key.is_empty() || saved.bpm <= 0.0 {
            return false;
        }
        if saved.version > SESSION_VERSION {
            crate::logln!("set saved by a newer chopsuey (session v{}); restoring what this version knows", saved.version);
        }
        if let Some(old) = saved.path.clone().filter(|p| !p.exists()) {
            // Moved or renamed folder, or a set from another computer: look
            // for it in the library by name (and size).
            let Some(found) = library::find_moved(&old, saved.file_size) else {
                crate::logln!("saved sample missing: {}", old.display());
                *self.render_error.locked() = Some(format!(
                    "Can't find \u{201c}{}\u{201d}, which this set uses. It was in {}. Add the folder it's in now and reopen the set, or pick another sample.",
                    saved.name,
                    old.parent().map_or_else(String::new, |d| d.display().to_string()),
                ));
                return false;
            };
            crate::logln!("saved sample moved: {} -> {}", old.display(), found.display());
            saved.key = found.to_string_lossy().into_owned();
            saved.path = Some(found);
            // Saving the set from now on stores the new place.
            *self.session.locked() = saved.clone();
            *self.applied_session.locked() = saved.clone();
        }
        if saved.chop_length > 0 {
            self.chop_length.store(saved.chop_length, Ordering::Relaxed);
        }
        if saved.stem_switches != 0 {
            self.stem_switches.store(saved.stem_switches, Ordering::Relaxed);
        }
        *self.selection.locked() = Selection {
            key: saved.key.clone(),
            path: saved.path.clone(),
            name: saved.name.clone(),
            bpm: saved.bpm,
            chop: None,
            stem_mask: ALL_STEMS,
        };
        if let Some((start, end)) = saved.chop {
            *self.restore_chop.locked() = Some((saved.key.clone(), start, end));
        }
        if saved.is_song {
            if let Some(path) = &saved.path {
                self.request_stems(&saved.key, path); // from the cache if split before
            }
        }
        let key = self.selection.locked().render_key();
        self.wanted_key.store(loops::key_hash(&key), Ordering::Relaxed);
        true
    }

    pub fn next_request(&self) -> u64 {
        self.latest_request.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// False while a requested render hasn't finished yet.
    pub fn is_up_to_date(&self) -> bool {
        self.rendered_request.load(Ordering::Relaxed) == self.latest_request.load(Ordering::Relaxed)
    }

    pub fn current(&self) -> Option<Arc<RenderedLoop>> {
        self.current.locked().clone()
    }

    /// The current render and its waveform.
    pub fn current_with_overview(&self) -> Option<(Arc<RenderedLoop>, Arc<Vec<(f32, f32)>>)> {
        let _guard = self.publish_lock.locked();
        Some((self.current()?, self.current_overview.locked().clone()))
    }

    /// Runs on nih-plug's background thread.
    fn render(self: &Arc<Self>, id: u64, bpm: f32) {
        if self.latest_request.load(Ordering::Relaxed) != id {
            return; // superseded while queued
        }
        while self.garbage.locked().pop().is_ok() {}

        let selection = self.selection.locked().clone();
        self.preview_gain.store(self.loudness_gain(&selection.key), Ordering::Relaxed);
        let asset = match self.load(&selection) {
            Ok(asset) => asset,
            Err(e) => {
                *self.render_error.locked() = Some(e);
                self.rendered_request.store(id, Ordering::Relaxed);
                return;
            }
        };
        *self.render_error.locked() = None;

        let span = self.span_of(&selection);
        let secs = asset.frames() as f32 / asset.sample_rate as f32;
        if !asset.is_loop && secs > LONG_RENDER_SECS {
            // Long: stretch on its own thread so a chop clicked meanwhile
            // doesn't wait behind it. The first few seconds are stretched and
            // published first so playback starts at once; the full render
            // replaces them without a jump (same tempo, same start).
            // Stale results are dropped in publish().
            let shared = self.clone();
            crate::safety::spawn("tempo-sampler-song", move || {
                let current = || shared.latest_request.load(Ordering::Relaxed) == id;
                let start_frames = (START_RENDER_SECS * asset.sample_rate as f32) as usize;
                if current() && asset.frames() > start_frames * 2 {
                    let mut start = shared.at_host_rate(asset.stretch_start(bpm, start_frames));
                    let part = start_frames as f32 / asset.frames() as f32;
                    start.span = span.map(|(a, b)| (a, a + (b - a) * part));
                    shared.publish(id, bpm, start);
                }
                // Gives up between pieces once a newer request exists, so
                // flicking through songs doesn't pile up stretches.
                if let Some(rendered) = asset.stretch_to_cancellable(bpm, &|| !current()) {
                    let mut rendered = shared.at_host_rate(rendered);
                    rendered.span = span;
                    shared.publish(id, bpm, rendered);
                }
            });
            // Find chops on another thread while it stretches (the whole
            // song plays without them), leaving this one free for the next
            // request (a chop click, say).
            if let Ok(src) = self.source(&selection) {
                let (shared, selection) = (self.clone(), selection.clone());
                crate::safety::spawn("tempo-sampler-chops", move || shared.analyse_if_song(&selection, &src));
            }
        } else {
            let mut rendered = self.at_host_rate(asset.stretch_to(bpm));
            rendered.span = span;
            self.publish(id, bpm, rendered);
        }
        // One-shots follow the stems that are on.
        self.update_one_shots(&selection);
        // Tiles show the mix of the stems that are on: work those waveforms
        // out here rather than in the GUI's next draw.
        if selection.stem_mask != ALL_STEMS {
            let _ = self.mixed_chop_overviews(selection.stem_mask);
        }
        // Selection, tempo or stems changed: the pads follow.
        self.refresh_pads();
    }

    /// Makes a finished render current, unless a newer request exists. A
    /// partial render (a song's start) plays but doesn't count as done.
    fn publish(&self, id: u64, bpm: f32, rendered: RenderedLoop) {
        // Outside the lock: the GUI takes it to draw.
        let overview = Arc::new(min_max_overview_stereo(&rendered.samples, OVERVIEW_POINTS));
        let _guard = self.publish_lock.locked();
        if self.latest_request.load(Ordering::Relaxed) != id {
            return; // superseded while stretching
        }
        // Free buffers the audio thread handed back (song renders publish
        // from their own thread, so render() alone may not keep up).
        while self.garbage.locked().pop().is_ok() {}
        let partial = rendered.partial;
        let rendered = Arc::new(rendered);
        *self.current.locked() = Some(rendered.clone());
        *self.current_overview.locked() = overview;
        // If the queue is full the audio thread isn't running; the GUI still
        // has the render, and the next one will get through.
        let _ = self.to_audio.locked().push(rendered);
        if !partial {
            self.rendered_bpm.store(bpm, Ordering::Relaxed);
            self.rendered_request.store(id, Ordering::Relaxed);
        }
    }

    /// Where the selection sits in its file, for the playhead.
    fn span_of(&self, selection: &Selection) -> Option<(f32, f32)> {
        let view = self.song_view.locked().clone();
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
        let mut cached = self.source.locked();
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
        if let Some((sel, asset)) = self.loaded.locked().as_ref() {
            if sel == selection && asset.pitch == self.pitch() {
                return Ok(asset.clone());
            }
        }
        let src = self.source(selection)?;
        let asset = Arc::new(self.make_asset(selection, &src)?);
        *self.loaded.locked() = Some((selection.clone(), asset.clone()));
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
        let one_shot = selection.chop.as_ref().is_some_and(|c| c.chop.bars == one_shots::ONE_SHOT);
        let (name, bpm) = match &selection.chop {
            Some(c) if one_shot => (format!("{} - one shot {}", selection.name, c.number), c.chop.bpm),
            Some(c) => (format!("{} - {} bar chop {}", selection.name, c.chop.bars, c.number), c.chop.bpm),
            None => (selection.name.clone(), selection.bpm),
        };
        let asset = LoopAsset::new(&selection.render_key(), &(name + &selection.stems_suffix()), bpm, src.sample_rate, samples)
            .with_pitch(self.pitch());
        Ok(if one_shot { asset.into_one_shot() } else { asset })
    }

    /// Runs chop analysis for a song (not a short loop) if it hasn't been
    /// done for this file and tempo yet.
    fn analyse_if_song(self: &Arc<Self>, selection: &Selection, src: &Source) {
        if selection.path.is_none() {
            return;
        }
        let secs = src.frames() as f32 / src.sample_rate as f32;
        if secs < SONG_MIN_SECS || bpm::is_loop(secs, selection.bpm) {
            return;
        }
        if let Some(view) = self.song_view.locked().as_ref() {
            if view.key == selection.key && view.bpm == selection.bpm {
                return;
            }
        }
        // One analysis per file and tempo at a time (renders come in bursts).
        let job = format!("{}@{}", selection.key, selection.bpm);
        {
            let mut running = self.analysing.locked();
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
        // One-shots in the full mix for now; `update_one_shots` redoes them
        // for the stems that are on.
        let mut chops = analysis.chops;
        let mut chop_overviews: Vec<_> = chop_overviews;
        for shot in one_shots::find_one_shots(&mono, src.sample_rate, selection.bpm) {
            chop_overviews.push(min_max_overview(&mono[shot.start..shot.end], CHOP_OVERVIEW_POINTS));
            chops.push(shot);
        }
        *self.song_view.locked() = Some(Arc::new(SongView {
            key: selection.key.clone(),
            bpm: selection.bpm,
            frames: src.frames(),
            sample_rate: src.sample_rate,
            overview,
            bar_starts: analysis.bar_starts,
            chops,
            chop_overviews,
            one_shot_mask: ALL_STEMS,
        }));
        self.restore_saved_chop();
        self.song_view_version.fetch_add(1, Ordering::Relaxed);
        let current = self.selection.locked().clone();
        self.update_one_shots(&current);
        {
            let mut running = self.analysing.locked();
            if running.as_deref() == Some(job.as_str()) {
                *running = None;
            }
        }
        self.refresh_pads(); // chops are known now
        self.save_session(); // now known to be a song (and maybe its chop)
    }

    /// A chop saved with the Live set: selects it again once a tile with
    /// its frames exists (bar chops after analysis; a one-shot maybe only
    /// once its stems are on).
    fn restore_saved_chop(&self) {
        let Some(view) = self.song_view.locked().clone() else { return };
        let mut restore = self.restore_chop.locked();
        let Some((key, start, end)) = restore.clone() else { return };
        if key != view.key || self.selection.locked().chop.is_some() {
            restore.take(); // another song, or a chop picked since: too late
            return;
        }
        // The same frames, or (if chop detection has changed since the set
        // was saved) the chop that overlaps the saved one most, if it
        // covers at least half of it.
        let overlap = |c: &chops::Chop| c.end.min(end).saturating_sub(c.start.max(start));
        let exact = view.chops.iter().position(|c| c.start == start && c.end == end);
        let closest = || {
            (0..view.chops.len())
                .max_by_key(|&i| overlap(&view.chops[i]))
                // Half of each: a one-shot inside a long bar chop isn't it.
                .filter(|&i| {
                    let (o, c) = (overlap(&view.chops[i]), &view.chops[i]);
                    o * 2 >= end.saturating_sub(start).max(1) && o * 2 >= (c.end - c.start).max(1)
                })
        };
        let Some(index) = exact.or_else(closest) else { return };
        restore.take();
        let chop = view.chops[index].clone();
        let number = view.chops[..index].iter().filter(|c| c.bars == chop.bars).count() + 1;
        let mut sel = self.selection.locked();
        if sel.key == key {
            sel.chop = Some(ChopSel { index, number, chop });
        }
    }

    /// Finds the selected song's one-shots again in the audio that's
    /// playing (the stems that are on), if that changed. Returns whether
    /// the song view changed.
    fn update_one_shots(&self, selection: &Selection) -> bool {
        let Some(view) = self.song_view.locked().clone().filter(|v| v.key == selection.key) else {
            return false;
        };
        let mask = selection.stem_mask;
        if view.one_shot_mask == mask {
            return false;
        }
        let Ok(src) = self.source(selection) else { return false };
        let full: Vec<f32> = src.samples.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
        let mixed: Vec<f32>;
        let heard = if mask == ALL_STEMS {
            &full
        } else {
            let Some(stems) = self.stems_for(&view.key) else { return false };
            mixed = stems::mix(&stems, mask, 0, view.frames).chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
            &mixed
        };
        let shots = one_shots::find_one_shots(heard, view.sample_rate, view.bpm);
        let bar_chops = view.chops.iter().take_while(|c| c.bars != one_shots::ONE_SHOT).count();
        let mut chops = view.chops[..bar_chops].to_vec();
        let mut chop_overviews = view.chop_overviews[..bar_chops].to_vec();
        for shot in shots {
            chop_overviews.push(min_max_overview(&full[shot.start..shot.end.min(full.len())], CHOP_OVERVIEW_POINTS));
            chops.push(shot);
        }
        let new = Arc::new(SongView {
            key: view.key.clone(),
            bpm: view.bpm,
            frames: view.frames,
            sample_rate: view.sample_rate,
            overview: view.overview.clone(),
            bar_starts: view.bar_starts.clone(),
            chops,
            chop_overviews,
            one_shot_mask: mask,
        });
        {
            let mut current = self.song_view.locked();
            if !current.as_ref().is_some_and(|c| Arc::ptr_eq(c, &view)) {
                return false; // replaced meanwhile
            }
            *current = Some(new.clone());
        }
        *self.mixed_overviews.locked() = None;
        // A selected one-shot keeps playing; its tile is found again if
        // it's still there.
        {
            let mut sel = self.selection.locked();
            if sel.key == new.key {
                if let Some(c) = sel.chop.as_mut().filter(|c| c.chop.bars == one_shots::ONE_SHOT) {
                    match new.chops.iter().position(|x| x.start == c.chop.start && x.end == c.chop.end) {
                        Some(i) => {
                            c.index = i;
                            c.number = i - bar_chops + 1;
                        }
                        None => c.index = usize::MAX,
                    }
                }
            }
        }
        self.restore_saved_chop();
        self.song_view_version.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// The selected song's stems, if they're ready.
    pub fn stems_for(&self, key: &str) -> Option<Arc<stems::Stems>> {
        self.stems.locked().as_ref().filter(|(k, _)| k == key).map(|(_, s)| s.clone())
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
        self.want_selection();
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
            *self.stem_state.locked() = (key.to_string(), StemState::Ready);
            return;
        }
        let cancel = {
            let mut job = self.stem_job.locked();
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
        *self.stem_state.locked() = (key.to_string(), StemState::Loading);
        self.stem_progress.store(0.0, Ordering::Relaxed);

        let shared = self.clone();
        let (key, path) = (key.to_string(), path.to_path_buf());
        crate::safety::spawn("tempo-sampler-stems", move || {
            let set_state = |state: StemState| {
                if !cancel.load(Ordering::Relaxed) {
                    *shared.stem_state.locked() = (key.clone(), state);
                }
            };
            let result = (|| -> Result<Option<stems::Stems>, String> {
                let dir = stems::cache_dir_for(&path);
                if let Some(cached) = dir.as_deref().and_then(stems::load_cached) {
                    return Ok(Some(cached));
                }
                set_state(StemState::Splitting);
                let dir = dir.ok_or("Can't read this file to split it.")?;
                // In the helper process (see stems.rs), into the cache.
                let finished = stems::split(
                    &path,
                    &dir,
                    |p| shared.stem_progress.store(p, Ordering::Relaxed),
                    || cancel.load(Ordering::Relaxed),
                )?;
                if !finished {
                    return Ok(None);
                }
                stems::load_cached(&dir).map(Some).ok_or_else(|| "The stems were split but couldn't be read back.".to_string())
            })();
            match result {
                Ok(Some(st)) if !cancel.load(Ordering::Relaxed) => {
                    *shared.stems.locked() = Some((key.clone(), Arc::new(st)));
                    set_state(StemState::Ready);
                    // Apply the switches if this song is still selected.
                    let switches = shared.stem_switches.load(Ordering::Relaxed);
                    let rerender = {
                        let mut sel = shared.selection.locked();
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
            let mut job = shared.stem_job.locked();
            if job.as_ref().is_some_and(|(k, _)| *k == key) {
                *job = None;
            }
        });
    }

    /// Waveform overviews of every chop, mixed from the enabled stems.
    /// Cached per song and mask; `None` if the stems aren't ready.
    pub fn mixed_chop_overviews(&self, mask: u8) -> Option<Arc<Vec<Vec<(f32, f32)>>>> {
        let view = self.song_view.locked().clone()?;
        if let Some((key, m, cached)) = self.mixed_overviews.locked().as_ref() {
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
        *self.mixed_overviews.locked() = Some((view.key.clone(), mask, overviews.clone()));
        Some(overviews)
    }

    /// Exports the current selection at the host tempo (the drag area and
    /// "Show WAV in Finder").
    pub fn export_current(&self) -> Result<PathBuf, String> {
        let selection = self.selection.locked().clone();
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
            if r.key == wanted && (r.bpm - bpm).abs() < 1e-3 && !r.partial && r.pitch == self.pitch() {
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
        self.write_export(Arc::new(self.at_host_rate(asset.stretch_to(bpm))))
    }

    /// Writes a render to the export folder. Song renders can be tens of
    /// MB, so the same render is written at most once.
    fn write_export(&self, rendered: Arc<RenderedLoop>) -> Result<PathBuf, String> {
        let mut exported = self.exported.locked();
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

/// What's saved in the Live set and restored when it reopens: the selected
/// file, its chop, the bar length shown and the stem switches.
#[derive(serde::Serialize, serde::Deserialize, Default, Clone, Debug, PartialEq)]
pub struct SessionState {
    pub key: String,
    pub path: Option<PathBuf>,
    pub name: String,
    pub bpm: f32,
    pub is_song: bool,
    /// The selected chop by its frames in the song (stable across loads:
    /// the analysis is deterministic).
    pub chop: Option<(usize, usize)>,
    pub chop_length: usize,
    pub stem_switches: u8,
    /// `SESSION_VERSION` when saved; 0 for sets saved before it existed.
    /// New fields must have serde defaults so older sets still load.
    #[serde(default)]
    pub version: u32,
    /// The file's size when saved, to recognise it if it has moved.
    #[serde(default)]
    pub file_size: Option<u64>,
}

/// Bump when `SessionState` changes meaning, and handle the older versions
/// in `restore_session`.
pub const SESSION_VERSION: u32 = 1;

/// Clips for MIDI pads, rendered ahead so a pad plays at once.
pub struct PadSet {
    /// Chop tiles of the shown length, in tile order: note 36 + k plays
    /// `clips[k]`. Empty for a loop or short sample (any pad plays the
    /// current clip) or while a song's set is still being rendered.
    pub clips: Vec<Arc<RenderedLoop>>,
    /// `song_view.chops` index of each clip, so the GUI can light its tile.
    pub chop_indices: Vec<usize>,
    /// Not a song: any pad plays whatever is selected.
    pub play_current: bool,
    /// Loudness-matching gain for these clips (see `Shared::preview_gain`).
    pub gain: f32,
}

/// First pad note (C1 in Live / most pad controllers).
const FIRST_PAD_NOTE: u8 = 36;

pub enum Task {
    Render { id: u64, bpm: f32 },
    /// Parameters changed (automation, MIDI mapping or the GUI): apply them.
    ApplyParams(ParamValues),
}

/// The parameters `Shared::apply_params` acts on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamValues {
    pub stem_mask: u8,
    pub chop_length: usize,
    pub tempo_scale: f32,
    pub pitch: Pitch,
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
    /// Transport state last block, to detect Ableton's stop button.
    was_playing: bool,
    /// Play position in frames of the rendered buffer. Every press starts
    /// the clip from its first sample (loops then repeat), rather than
    /// joining in wherever the bar grid is.
    play_pos: f64,
    /// Preview state last block, to restart the clip when a press starts.
    was_preview_on: bool,
    /// Smoothed preview on/off level, 0.0..=1.0.
    preview_level: f32,
    preview_fade_coef: f32,
    // MIDI pads.
    from_pads: rtrb::Consumer<Arc<PadSet>>,
    to_pad_garbage: rtrb::Producer<Arc<PadSet>>,
    pads: Option<Arc<PadSet>>,
    voices: [Option<Voice>; MAX_VOICES],
    /// Slot of the most recent hit, for the playhead.
    last_voice: usize,
    /// `process` panicked: output silence until the next `initialize`.
    faulted: bool,
    /// Parameter values last handed to `apply_params`.
    applied_params: ParamValues,
}

#[derive(Params)]
pub struct TempoSamplerParams {
    /// Window size, saved with the Ableton set.
    #[persist = "editor-state"]
    pub editor_state: Arc<ViziaState>,

    /// The selection etc., saved with the set (see `SessionState`).
    #[persist = "session"]
    session: Arc<Mutex<SessionState>>,

    /// Mono: a new pad cuts off the one playing (like a choke group).
    /// Poly: pads overlap, up to `MAX_VOICES`.
    #[id = "pad_mode"]
    pub pad_mode: EnumParam<PadMode>,

    // The controls, as parameters Live can automate and MIDI-map. Changes
    // from the host are applied by `Shared::apply_params`; the GUI sets
    // them too, so automation lanes follow clicks.
    #[id = "drums"]
    pub drums: BoolParam,
    #[id = "bass"]
    pub bass: BoolParam,
    #[id = "other"]
    pub other: BoolParam,
    #[id = "vocals"]
    pub vocals: BoolParam,
    #[id = "chop_len"]
    pub chop_length: EnumParam<ChopLength>,
    #[id = "tempo_x"]
    pub tempo_scale: EnumParam<TempoScale>,
    /// Play previews and pads at a matched loudness (exports unchanged).
    #[id = "match_loud"]
    pub match_loudness: BoolParam,
    /// Pitch shift for everything (previews, pads, dragged files), like
    /// Ableton's clip Transpose and Detune. Tempo is unaffected.
    #[id = "transpose"]
    pub transpose: IntParam,
    #[id = "detune"]
    pub detune: IntParam,
    /// Flipped by the GUI whenever a file or chop is picked. It does nothing
    /// itself: the pick lives in `session`, which Live doesn't watch, so
    /// without a parameter change Live wouldn't know the set changed and
    /// could quit without offering to save it.
    #[id = "picked"]
    pub picked: BoolParam,
}

/// "+3", "-12", "0": how Ableton shows Transpose and Detune.
pub fn signed(v: i32) -> String {
    if v == 0 { "0".into() } else { format!("{v:+}") }
}

impl TempoSamplerParams {
    /// Stem switches as a mask (bit i = `stems::STEM_NAMES[i]`).
    pub fn stem_mask(&self) -> u8 {
        [&self.drums, &self.bass, &self.other, &self.vocals]
            .iter()
            .enumerate()
            .fold(0, |m, (i, p)| if p.value() { m | 1 << i } else { m })
    }

    pub fn pitch(&self) -> Pitch {
        Pitch { semitones: self.transpose.value(), cents: self.detune.value() }
    }
}

#[derive(Enum, Debug, PartialEq, Clone, Copy)]
pub enum ChopLength {
    #[name = "1 bar"]
    One,
    #[name = "2 bars"]
    Two,
    #[name = "4 bars"]
    Four,
    /// Single sounds instead of bar-aligned loops (see `one_shots`).
    #[name = "One shots"]
    OneShots,
}

impl ChopLength {
    pub fn bars(self) -> usize {
        match self {
            ChopLength::One => 1,
            ChopLength::Two => 2,
            ChopLength::Four => 4,
            ChopLength::OneShots => one_shots::ONE_SHOT,
        }
    }

    pub fn from_bars(bars: usize) -> Self {
        match bars {
            1 => ChopLength::One,
            4 => ChopLength::Four,
            one_shots::ONE_SHOT => ChopLength::OneShots,
            _ => ChopLength::Two,
        }
    }
}

#[derive(Enum, Debug, PartialEq, Clone, Copy)]
pub enum TempoScale {
    #[name = "½×"]
    Half,
    #[name = "1×"]
    One,
    #[name = "2×"]
    Double,
}

impl TempoScale {
    pub fn factor(self) -> f32 {
        match self {
            TempoScale::Half => 0.5,
            TempoScale::One => 1.0,
            TempoScale::Double => 2.0,
        }
    }

    pub fn from_factor(f: f32) -> Self {
        if f < 0.75 {
            TempoScale::Half
        } else if f > 1.5 {
            TempoScale::Double
        } else {
            TempoScale::One
        }
    }
}

#[derive(Enum, Debug, PartialEq, Clone, Copy)]
pub enum PadMode {
    #[name = "Mono (choke)"]
    Mono,
    #[name = "Poly"]
    Poly,
}

/// Pad voices that can sound at once in Poly mode.
const MAX_VOICES: usize = 8;

/// One pad playing a clip from its start.
struct Voice {
    note: u8,
    clip: Arc<RenderedLoop>,
    /// `song_view.chops` index of its tile, or -1.
    chop: i64,
    pos: f64,
    /// Smoothed on/off level (fades in on the hit, out on release).
    level: f32,
    /// From the note's velocity.
    gain: f32,
    held: bool,
}

impl Default for TempoSampler {
    fn default() -> Self {
        crate::safety::install_panic_hook();
        let (to_audio, from_render) = rtrb::RingBuffer::new(8);
        let (to_garbage, garbage) = rtrb::RingBuffer::new(16);
        let (to_audio_pads, from_pads) = rtrb::RingBuffer::new(4);
        let session = Arc::new(Mutex::new(SessionState::default()));
        let (to_pad_garbage, pad_garbage) = rtrb::RingBuffer::new(8);
        let shared = Shared {
            transport: HostTransport::default(),
            library: Arc::new(Library::default()),
            selection: Mutex::new(Selection::builtin()),
            source: Mutex::new(None),
            loaded: Mutex::new(None),
            song_view: Mutex::new(None),
            song_view_version: AtomicU64::new(0),
            analysing: Mutex::new(None),
            wanted_key: AtomicU64::new(0),
            pads_sig: Mutex::new(String::new()),
            to_audio_pads: Mutex::new(to_audio_pads),
            pad_garbage: Mutex::new(pad_garbage),
            pads_down: AtomicU64::new(0),
            audio_fault: AtomicBool::new(false),
            host_rate: std::sync::atomic::AtomicU32::new(0),
            transpose: AtomicI32::new(0),
            detune: AtomicI32::new(0),
            preview_gain: AtomicF32::new(1.0),
            session: session.clone(),
            applied_session: Mutex::new(SessionState::default()),
            restore_chop: Mutex::new(None),
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
                session,
                pad_mode: EnumParam::new("Pad mode", PadMode::Mono),
                drums: BoolParam::new("Drums", true),
                bass: BoolParam::new("Bass", true),
                other: BoolParam::new("Other", true),
                vocals: BoolParam::new("Vocals", true),
                chop_length: EnumParam::new("Chop length", ChopLength::Two),
                tempo_scale: EnumParam::new("Tempo", TempoScale::One),
                match_loudness: BoolParam::new("Match loudness", true),
                transpose: IntParam::new("Transpose", 0, IntRange::Linear { min: -48, max: 48 })
                    .with_unit(" st")
                    .with_value_to_string(Arc::new(signed)),
                detune: IntParam::new("Detune", 0, IntRange::Linear { min: -50, max: 50 })
                    .with_unit(" ct")
                    .with_value_to_string(Arc::new(signed)),
                picked: BoolParam::new("Sample pick", false),
            }),
            shared: Arc::new(shared),
            from_render,
            to_garbage,
            playing_loop: None,
            sample_rate: 48_000.0,
            requested_bpm: 0.0,
            pending_bpm: 0.0,
            pending_samples: 0,
            was_playing: false,
            play_pos: 0.0,
            was_preview_on: false,
            preview_level: 0.0,
            preview_fade_coef: 1.0,
            from_pads,
            to_pad_garbage,
            pads: None,
            voices: std::array::from_fn(|_| None),
            last_voice: 0,
            faulted: false,
            applied_params: ParamValues { stem_mask: ALL_STEMS, chop_length: 2, tempo_scale: 1.0, pitch: Pitch::NONE },
        }
    }
}

impl TempoSampler {
    fn param_values(&self) -> ParamValues {
        let p = &self.params;
        ParamValues {
            stem_mask: p.stem_mask(),
            chop_length: p.chop_length.value().bars(),
            tempo_scale: p.tempo_scale.value().factor(),
            pitch: p.pitch(),
        }
    }

    /// A pad was hit: start its clip from the top at `velocity` (0..1).
    /// Song pads play their chop tile; otherwise the current clip. In Mono
    /// mode the pads already playing are released (choked).
    fn pad_on(&mut self, note: u8, velocity: f32) {
        let (clip, chop) = match self.pads.as_deref() {
            Some(set) if !set.play_current => {
                let k = note.checked_sub(FIRST_PAD_NOTE).map(usize::from);
                match k.and_then(|k| Some((set.clips.get(k)?.clone(), set.chop_indices[k] as i64))) {
                    Some(hit) => hit,
                    None => return, // no tile for this pad (or still rendering)
                }
            }
            _ => match self.playing_loop.clone() {
                Some(clip) => (clip, -1),
                None => return,
            },
        };
        if self.params.pad_mode.value() == PadMode::Mono {
            for v in self.voices.iter_mut().flatten() {
                v.held = false;
            }
        }
        // A free slot, else the quietest voice (released ones first).
        let slot = self.voices.iter().position(|v| v.is_none()).unwrap_or_else(|| {
            (0..MAX_VOICES)
                .min_by(|&a, &b| {
                    let key = |i: usize| self.voices[i].as_ref().map_or((false, 0.0), |v| (v.held, v.level));
                    let (ka, kb) = (key(a), key(b));
                    (ka.0, ka.1).partial_cmp(&(kb.0, kb.1)).unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(0)
        });
        let loudness = if !self.params.match_loudness.value() {
            1.0
        } else if chop >= 0 {
            self.pads.as_deref().map_or(1.0, |set| set.gain)
        } else {
            self.shared.preview_gain.load(Ordering::Relaxed)
        };
        let voice = Voice { note, clip, chop, pos: 0.0, level: 0.0, gain: velocity.clamp(0.0, 1.0) * loudness, held: true };
        if let Some(old) = self.voices[slot].replace(voice) {
            let _ = self.to_garbage.push(old.clip); // freed off the audio thread
        }
        self.last_voice = slot;
        self.publish_pads_down();
    }

    /// A pad was released: its voices fade out. One-shots play to their end
    /// (like a Drum Rack's one-shot samples), unless choked in Mono mode.
    fn pad_off(&mut self, note: u8) {
        for v in self.voices.iter_mut().flatten() {
            if v.note == note && !v.clip.one_shot {
                v.held = false;
            }
        }
        self.publish_pads_down();
    }

    /// Tells the GUI which tiles have their pad held.
    fn publish_pads_down(&self) {
        let mask = self
            .voices
            .iter()
            .flatten()
            .filter(|v| v.held && (0..64).contains(&v.chop))
            .fold(0u64, |m, v| m | 1 << v.chop);
        self.shared.pads_down.store(mask, Ordering::Relaxed);
    }
}

impl Plugin for TempoSampler {
    const NAME: &'static str = "chopsuey";
    const VENDOR: &'static str = "Toronto Audio Company";
    // Release blocker: the company site and support address go here.
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    // An instrument: stereo out, no audio in. Previews (mouse or MIDI pad)
    // are its output.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];
    // Pads: note 36 (C1) plays the first chop tile, 37 the second, ...
    const MIDI_INPUT: MidiConfig = MidiConfig::Basic;

    type SysExMessage = ();
    type BackgroundTask = Task;

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn task_executor(&mut self) -> TaskExecutor<Self> {
        let shared = self.shared.clone();
        Box::new(move |task| {
            crate::safety::guard("background task", || match task {
                Task::Render { id, bpm } => shared.render(id, bpm),
                Task::ApplyParams(values) => shared.apply_params(values),
            });
        })
    }

    fn editor(&mut self, async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(
            self.shared.clone(),
            async_executor,
            self.params.clone(),
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
        // Renders are made at the host's rate: if that changed, re-render
        // (the audio thread asks once it sees a tempo).
        let rate = self.sample_rate.round() as u32;
        if self.shared.host_rate.swap(rate, Ordering::Relaxed) != rate {
            self.requested_bpm = 0.0;
        }
        // Put back the Live set's saved state if the host has loaded some
        // since the last call. Live activates the plugin (one initialize)
        // before loading the set's state (another initialize), so this can't
        // be a first-call-only check. Sample-rate changes etc. find nothing new.
        if self.shared.restore_if_loaded() {
            self.requested_bpm = 0.0; // render the restored selection
        }
        // Parameter values as loaded are what's in effect (the session state
        // saved with them agrees); only later changes need applying. The
        // pitch lives only in its parameters: take it from them, and
        // re-render if it differs from what was playing.
        self.applied_params = self.param_values();
        // Recover from an audio-thread panic: start again with no voices.
        if self.faulted {
            self.faulted = false;
            self.shared.audio_fault.store(false, Ordering::Relaxed);
            for v in self.voices.iter_mut() {
                if let Some(v) = v.take() {
                    let _ = self.to_garbage.push(v.clip);
                }
            }
        }
        if self.shared.set_pitch(self.applied_params.pitch) {
            self.requested_bpm = 0.0;
        }
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // A panic unwinding into the host would abort it (Live quits, the
        // set is lost). Catch it, go silent, and stay silent until the host
        // re-initialises the plugin. Costs nothing unless something panics.
        if !self.faulted {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.process_block(buffer, context)));
            match result {
                Ok(status) => return status,
                Err(_) => {
                    self.faulted = true;
                    self.shared.audio_fault.store(true, Ordering::Relaxed);
                }
            }
        }
        for channel in buffer.as_slice() {
            channel.fill(0.0);
        }
        ProcessStatus::Normal
    }
}

impl TempoSampler {
    /// One block of audio: what `process` runs, inside its panic guard.
    fn process_block(&mut self, buffer: &mut Buffer, context: &mut impl ProcessContext<Self>) -> ProcessStatus {
        // An instrument: the output starts silent and previews are added.
        for channel in buffer.as_slice() {
            channel.fill(0.0);
        }

        // 0. MIDI pads: take the newest pad set, then this block's notes.
        while let Ok(set) = self.from_pads.pop() {
            if let Some(old) = self.pads.replace(set) {
                let _ = self.to_pad_garbage.push(old); // freed off the audio thread
            }
        }
        // Notes are applied at their exact sample in step 4b. A fixed-size
        // list: no allocation on the audio thread (extra notes are dropped).
        let mut notes = [(0u32, 0u8, 0.0f32, false); 128];
        let mut note_count = 0;
        while let Some(event) = context.next_event() {
            let note = match event {
                NoteEvent::NoteOn { timing, note, velocity, .. } => (timing, note, velocity, true),
                NoteEvent::NoteOff { timing, note, .. } => (timing, note, 0.0, false),
                _ => continue,
            };
            if note_count < notes.len() {
                notes[note_count] = note;
                note_count += 1;
            }
        }

        // Parameters changed (automation, MIDI mapping, the GUI): apply them
        // off the audio thread.
        let values = self.param_values();
        if values != self.applied_params {
            self.applied_params = values;
            context.execute_background(Task::ApplyParams(values));
        }

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
            self.play_pos = match self.playing_loop.as_deref() {
                Some(old) if old.key == new_loop.key && new_loop.bpm > 0.0 => {
                    self.play_pos * old.bpm as f64 / new_loop.bpm as f64
                }
                _ => 0.0,
            };
            if let Some(old) = self.playing_loop.replace(new_loop) {
                // Hand the old buffer back so it's freed off the audio thread.
                // If that queue is somehow full, dropping here is the fallback.
                let _ = self.to_garbage.push(old);
            }
        }

        // 4. Mix the preview: from the clip's start, looping, at its
        // stretched tempo.
        // Ableton's stop button stops the preview too. Starting the preview
        // while stopped still works (it free-runs) for auditioning.
        if self.was_playing && !t.playing {
            shared.preview.store(false, Ordering::Relaxed);
        }
        self.was_playing = t.playing;
        let preview_on = shared.preview.load(Ordering::Relaxed);
        if preview_on && !self.was_preview_on {
            self.play_pos = 0.0; // each press starts at the top
        }
        self.was_preview_on = preview_on;
        // Only play the clip that was asked for: right after a press the
        // previous clip is still loaded until the new render arrives.
        let wanted = shared.wanted_key.load(Ordering::Relaxed);
        let ready = self
            .playing_loop
            .as_deref()
            .is_some_and(|lp| lp.frames() > 0 && (wanted == 0 || lp.key_hash == wanted));
        if !ready {
            self.preview_level = 0.0; // fade in once it arrives
        }

        let preview_gain =
            if self.params.match_loudness.value() { shared.preview_gain.load(Ordering::Relaxed) } else { 1.0 };
        for mut frame in buffer.iter_samples() {
            if !ready {
                break;
            }
            let target = if preview_on { 1.0 } else { 0.0 };
            self.preview_level += (target - self.preview_level) * self.preview_fade_coef;
            if self.preview_level < 1e-5 {
                self.preview_level = 0.0;
                continue;
            }
            let Some(lp) = self.playing_loop.as_deref() else { break };
            let frames = lp.frames();
            let pos = self.play_pos;
            let step = lp.sample_rate as f64 / self.sample_rate as f64;
            if lp.one_shot && pos >= (frames - 1) as f64 {
                continue; // a one-shot plays once
            }
            self.play_pos = if lp.one_shot { pos + step } else { (pos + step) % frames as f64 };
            let i0 = (pos as usize).min(frames - 1);
            let i1 = if lp.one_shot { (i0 + 1).min(frames - 1) } else { (i0 + 1) % frames };
            let frac = (pos - i0 as f64) as f32;
            // Unity gain, or loudness-matched: the level of what lands in the
            // set is up to the track.
            let level = self.preview_level * preview_gain;
            for ch in 0..2 {
                let s = lp.samples[i0 * 2 + ch] * (1.0 - frac) + lp.samples[i1 * 2 + ch] * frac;
                if let Some(o) = frame.get_mut(ch) {
                    *o += s * level;
                }
            }
        }

        // 4b. Pad voices: each plays its clip from the start while its pad
        // is held and fades out on release. Notes start on their own sample.
        let mut next_note = 0;
        for (i, mut frame) in buffer.iter_samples().enumerate() {
            while next_note < note_count && notes[next_note].0 as usize <= i {
                let (_, note, velocity, on) = notes[next_note];
                if on {
                    self.pad_on(note, velocity);
                } else {
                    self.pad_off(note);
                }
                next_note += 1;
            }
            let coef = self.preview_fade_coef;
            for slot in 0..MAX_VOICES {
                let Some(v) = self.voices[slot].as_mut() else { continue };
                let target = if v.held { 1.0 } else { 0.0 };
                v.level += (target - v.level) * coef;
                let frames = v.clip.frames();
                let one_shot = v.clip.one_shot;
                let played_out = one_shot && v.pos >= (frames.max(1) - 1) as f64;
                if frames == 0 || (!v.held && v.level < 1e-4) || played_out {
                    if let Some(done) = self.voices[slot].take() {
                        let _ = self.to_garbage.push(done.clip);
                    }
                    if played_out {
                        self.publish_pads_down(); // its tile goes dark
                    }
                    continue;
                }
                let pos = v.pos;
                let step = v.clip.sample_rate as f64 / self.sample_rate as f64;
                v.pos = if one_shot { pos + step } else { (pos + step) % frames as f64 };
                let i0 = (pos as usize).min(frames - 1);
                let i1 = if one_shot { (i0 + 1).min(frames - 1) } else { (i0 + 1) % frames };
                let frac = (pos - i0 as f64) as f32;
                let g = v.level * v.gain;
                for ch in 0..2 {
                    let s = v.clip.samples[i0 * 2 + ch] * (1.0 - frac) + v.clip.samples[i1 * 2 + ch] * frac;
                    if let Some(o) = frame.get_mut(ch) {
                        *o += s * g;
                    }
                }
            }
        }
        // Notes past the end of the block (shouldn't happen) still count.
        while next_note < note_count {
            let (_, note, velocity, on) = notes[next_note];
            if on { self.pad_on(note, velocity) } else { self.pad_off(note) }
            next_note += 1;
        }
        let shared = &self.shared;

        // 5. Report where the preview is within the selected song (the
        // latest pad's clip first).
        if let Some(v) = self.voices[self.last_voice].as_ref() {
            if let (Some((a, b)), true) = (v.clip.span, v.clip.frames() > 0) {
                let p = a + (v.pos / v.clip.frames() as f64) as f32 * (b - a);
                shared.playhead.store(p, Ordering::Relaxed);
                return ProcessStatus::Normal;
            }
        }
        let playhead = match self.playing_loop.as_deref() {
            Some(lp) if ready && self.preview_level > 1e-3 => match lp.span {
                Some((a, b)) => a + (self.play_pos / lp.frames() as f64) as f32 * (b - a),
                None => -1.0,
            },
            _ => -1.0,
        };
        shared.playhead.store(playhead, Ordering::Relaxed);

        ProcessStatus::Normal
    }
}

impl ClapPlugin for TempoSampler {
    // Hosts identify the plugin by this: never change it after release.
    const CLAP_ID: &'static str = "com.torontoaudiocompany.chopsuey";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Browse, chop and stretch samples to your set's tempo");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::Instrument,
        ClapFeature::Sampler,
        ClapFeature::Stereo,
    ];
}

impl Vst3Plugin for TempoSampler {
    // Must be exactly 16 bytes and never change once people use the plugin,
    // or Ableton will treat it as a different plugin. It keeps the old name
    // on purpose so sets made before the rename still find it.
    const VST3_CLASS_ID: [u8; 16] = *b"TempoSamplerV001";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Instrument, Vst3SubCategory::Sampler, Vst3SubCategory::Stereo];
}

nih_export_clap!(TempoSampler);
nih_export_vst3!(TempoSampler);

#[cfg(test)]
mod tests {
    use super::*;

    /// Sets saved before the session had a version still load.
    #[test]
    fn old_session_state_loads() {
        let old = r#"{"key":"/x/a.wav","path":"/x/a.wav","name":"a","bpm":120.0,"is_song":false,"chop":[10,20],"chop_length":2,"stem_switches":15}"#;
        let s: SessionState = serde_json::from_str(old).unwrap();
        assert_eq!((s.version, s.file_size, s.chop), (0, None, Some((10, 20))));
    }
    use std::time::{Duration, Instant};

    fn wait_rendered(shared: &Shared, id: u64) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while shared.rendered_request.load(Ordering::Relaxed) != id {
            assert!(Instant::now() < deadline, "render {id} did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn one_shots_are_found_and_play_at_their_own_speed() {
        // A 70 s "song": the test loop (bar chops), then shouts with pauses.
        let sr = 48_000usize;
        let looped = LoopAsset::test_loop().stretch_to(97.0).samples;
        let mut samples = Vec::new();
        while samples.len() < 2 * sr * 50 {
            samples.extend_from_slice(&looped);
        }
        for k in 0..8 {
            samples.extend(std::iter::repeat(0.0).take(2 * sr)); // 1 s pause
            let len = sr * (5 + k) / 10; // 0.5 .. 1.2 s
            samples.extend((0..len).flat_map(|i| {
                let v = 0.4 * (std::f32::consts::TAU * 300.0 * i as f32 / sr as f32).sin();
                [v, v]
            }));
        }
        samples.extend(std::iter::repeat(0.0).take(2 * sr * 4));
        let dir = std::env::temp_dir().join(format!("tempo-sampler-shots-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shouts.wav");
        let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        samples.iter().for_each(|s| w.write_sample(*s).unwrap());
        w.finalize().unwrap();

        let plugin = TempoSampler::default();
        let shared = plugin.shared.clone();
        *shared.selection.locked() = Selection {
            key: path.to_string_lossy().into(),
            path: Some(path.clone()),
            name: "shouts".into(),
            bpm: 97.0,
            chop: None,
            stem_mask: ALL_STEMS,
        };
        let src = shared.source(&shared.selection.locked().clone()).unwrap();
        let selection = shared.selection.locked().clone();
        shared.analyse_if_song(&selection, &src);
        let view = shared.song_view.locked().clone().expect("analysed");
        let shots: Vec<usize> = (0..view.chops.len()).filter(|&i| view.chops[i].bars == one_shots::ONE_SHOT).collect();
        assert!(shots.len() >= 8, "{} one-shots", shots.len());
        assert!(view.chops.iter().any(|c| c.bars == 2), "bar chops too");
        assert_eq!(view.chop_overviews.len(), view.chops.len());
        // Bar chops first, one-shots after.
        assert!(shots.iter().all(|&i| i >= view.chops.len() - shots.len()));

        // The last shout (1.2 s), at 140 BPM: its own length, plays once.
        let i = *shots.last().unwrap();
        let chop = view.chops[i].clone();
        shared.selection.locked().chop = Some(ChopSel { index: i, number: shots.len(), chop: chop.clone() });
        let id = shared.next_request();
        shared.render(id, 140.0);
        let r = shared.current().unwrap();
        assert!(r.one_shot && !r.is_loop);
        assert_eq!(r.frames(), chop.end - chop.start);
        let secs = r.frames() as f32 / sr as f32;
        assert!((secs - 1.2).abs() < 0.1, "{secs} s");
        assert_eq!(r.file_name(), format!("shouts - one shot {}.wav", shots.len()));
        // Same stems: nothing to redo.
        assert!(!shared.update_one_shots(&shared.selection.locked().clone()));
    }

    #[test]
    fn transpose_param_rerenders_at_the_new_pitch() {
        let plugin = TempoSampler::default();
        let shared = plugin.shared.clone();
        shared.transport.tempo.store(100.0, Ordering::Relaxed);
        let mut values = plugin.param_values();
        let id = shared.next_request();
        shared.render(id, 100.0);
        assert_eq!(shared.current().unwrap().pitch, Pitch::NONE);
        let plain = shared.current().unwrap();

        values.pitch = Pitch { semitones: 3, cents: -20 };
        shared.apply_params(values);
        let pitched = shared.current().unwrap();
        assert_eq!(pitched.pitch, values.pitch);
        assert_eq!(pitched.frames(), plain.frames(), "tempo unchanged");
        assert!(pitched.samples != plain.samples);
        assert!(pitched.file_name().ends_with("100 BPM +3st -20ct.wav"), "{}", pitched.file_name());

        // Applying the same values again does nothing.
        shared.apply_params(values);
        assert!(Arc::ptr_eq(&pitched, &shared.current().unwrap()));
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
        *shared.selection.locked() = Selection {
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
        while shared.song_view.locked().is_none() {
            assert!(Instant::now() < deadline, "song not analysed");
            std::thread::sleep(Duration::from_millis(5));
        }
        let view = shared.song_view.locked().clone().expect("song analysed");
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
        shared.selection.locked().chop = Some(ChopSel { index: 0, number: 1, chop: chop.clone() });
        let id = shared.next_request();
        shared.render(id, 120.0);
        wait_rendered(&shared, id);
        let r = shared.current().unwrap();
        assert!(r.is_loop);
        assert_eq!(r.key, shared.selection.locked().render_key());
        assert_eq!(r.name, format!("song - {} bar chop 1", chop.bars));
        let bar_frames = 4.0 * 60.0 / 120.0 * 48_000.0;
        let bars = r.frames() as f32 / bar_frames;
        assert!((bars - chop.bars as f32).abs() < 0.001, "{bars} bars, want {}", chop.bars);
        let (a, b) = r.span.unwrap();
        assert!(a < b && b <= 1.0);

        // Dragging a chop that hasn't been rendered stretches it on the spot.
        std::env::set_var("TEMPO_SAMPLER_EXPORT_DIR", dir.join("export"));
        let other = view.chops.iter().position(|c| c.bars != chop.bars || c.start != chop.start).unwrap();
        let mut sel = shared.selection.locked().clone();
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

    /// MIDI pads: a song's chop tiles of the shown length are rendered ahead
    /// in tile order, and note 36 + k starts tile k from its first sample.
    #[test]
    fn midi_pads_play_chop_tiles() {
        let looped = LoopAsset::test_loop().stretch_to(97.0).samples;
        let mut samples = vec![0.0f32; 2 * 24_000];
        while samples.len() < 2 * 48_000 * 40 {
            samples.extend_from_slice(&looped);
        }
        let dir = std::env::temp_dir().join(format!("tempo-sampler-pads-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.wav");
        let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        samples.iter().for_each(|s| w.write_sample(*s).unwrap());
        w.finalize().unwrap();

        let mut plugin = TempoSampler::default();
        let shared = plugin.shared.clone();
        shared.transport.tempo.store(120.0, Ordering::Relaxed);
        *shared.selection.locked() = Selection {
            key: path.to_string_lossy().into(),
            path: Some(path.clone()),
            name: "song".into(),
            bpm: 97.0,
            chop: None,
            stem_mask: ALL_STEMS,
        };
        let id = shared.next_request();
        shared.render(id, 120.0);

        // The full set arrives after an empty "rendering" one.
        let deadline = Instant::now() + Duration::from_secs(30);
        let set = loop {
            assert!(Instant::now() < deadline, "no pad set");
            match plugin.from_pads.pop() {
                Ok(set) if !set.clips.is_empty() => break set,
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        let view = shared.song_view.locked().clone().unwrap();
        let length = shared.chop_length.load(Ordering::Relaxed);
        let tiles: Vec<usize> = (0..view.chops.len()).filter(|&i| view.chops[i].bars == length).collect();
        assert_eq!(set.chop_indices, tiles, "pads follow tile order");
        let bar_frames = 4.0 * 60.0 / 120.0 * 48_000.0;
        for clip in &set.clips {
            assert!(clip.is_loop);
            assert!((clip.frames() as f32 / bar_frames - length as f32).abs() < 0.001);
        }

        // Note 36 plays the first tile from the top; 35 does nothing.
        plugin.pads = Some(set.clone());
        plugin.pad_on(35, 1.0);
        assert!(plugin.voices.iter().all(|v| v.is_none()));
        plugin.pad_on(36, 0.5);
        let v = plugin.voices[plugin.last_voice].as_ref().unwrap();
        assert_eq!((v.note, v.chop, v.pos, v.gain), (36, tiles[0] as i64, 0.0, 0.5));
        assert!(Arc::ptr_eq(&v.clip, &set.clips[0]));
        assert_eq!(shared.pads_down.load(Ordering::Relaxed), 1 << tiles[0]);

        // Mono (the default): the next pad chokes the first. Poly: both play.
        plugin.pad_on(37, 1.0);
        assert_eq!(plugin.voices.iter().flatten().filter(|v| v.held).count(), 1);
        assert_eq!(shared.pads_down.load(Ordering::Relaxed), 1 << tiles[1]);
        plugin.pad_off(37);
        assert_eq!(shared.pads_down.load(Ordering::Relaxed), 0);
        assert!(plugin.voices.iter().flatten().all(|v| !v.held), "released voices fade out");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What a Live set saves comes back when it's reopened: the song, its
    /// chop (after analysis), the bar length and the stem switches.
    #[test]
    fn session_saved_with_the_set_is_restored() {
        let looped = LoopAsset::test_loop().stretch_to(97.0).samples;
        let mut samples = vec![0.0f32; 2 * 24_000];
        while samples.len() < 2 * 48_000 * 40 {
            samples.extend_from_slice(&looped);
        }
        let dir = std::env::temp_dir().join(format!("tempo-sampler-session-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.wav");
        let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        samples.iter().for_each(|s| w.write_sample(*s).unwrap());
        w.finalize().unwrap();
        let wait_view = |shared: &Shared| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while shared.song_view.locked().is_none() {
                assert!(Instant::now() < deadline, "not analysed");
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        // Session 1: pick the song, its third chop, 4 bars, drums off.
        let first = TempoSampler::default();
        let shared = first.shared.clone();
        *shared.selection.locked() = Selection {
            key: path.to_string_lossy().into(),
            path: Some(path.clone()),
            name: "song".into(),
            bpm: 97.0,
            chop: None,
            stem_mask: ALL_STEMS,
        };
        let id = shared.next_request();
        shared.render(id, 120.0);
        wait_view(&shared);
        let view = shared.song_view.locked().clone().unwrap();
        let chop = view.chops[2].clone();
        shared.selection.locked().chop = Some(ChopSel { index: 2, number: 3, chop: chop.clone() });
        shared.chop_length.store(4, Ordering::Relaxed);
        shared.stem_switches.store(0b1110, Ordering::Relaxed);
        shared.want_selection();
        // What Live stores: the persisted field, serialised.
        let saved = serde_json::to_string(&*first.params.session.locked()).unwrap();

        // Session 2: a fresh plugin, in Live's order: activated (initialize
        // with nothing loaded yet), then given the set's state (initialize
        // again).
        let second = TempoSampler::default();
        let shared2 = second.shared.clone();
        assert!(!shared2.restore_if_loaded(), "nothing loaded yet");
        let mut state: SessionState = serde_json::from_str(&saved).unwrap();
        assert!(state.is_song);
        // Don't start a stem split here: it would share the GPU with the
        // stems test running alongside (the plugin only ever runs one).
        state.is_song = false;
        *second.params.session.locked() = state;
        assert!(shared2.restore_if_loaded(), "state loaded after activation");
        assert!(!shared2.restore_if_loaded(), "a later initialize changes nothing");
        assert_eq!(shared2.selection.locked().key, path.to_string_lossy());
        assert_eq!(shared2.chop_length.load(Ordering::Relaxed), 4);
        assert_eq!(shared2.stem_switches.load(Ordering::Relaxed), 0b1110);
        let id = shared2.next_request();
        shared2.render(id, 120.0);
        wait_view(&shared2);
        let deadline = Instant::now() + Duration::from_secs(10);
        while shared2.selection.locked().chop.is_none() {
            assert!(Instant::now() < deadline, "chop not restored");
            std::thread::sleep(Duration::from_millis(5));
        }
        let restored = shared2.selection.locked().chop.clone().unwrap();
        assert_eq!((restored.index, restored.chop.start, restored.chop.end), (2, chop.start, chop.end));
        let _ = std::fs::remove_dir_all(&dir);
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

            *shared.source.locked() = None;
            *shared.song_view.locked() = None;
            *shared.selection.locked() = Selection {
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
            let want = shared.selection.locked().render_key();
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
            let sel = shared.selection.locked().clone();
            let t = Instant::now();
            let exported = shared.export_for_drag(&sel, 121.0);
            eprintln!("drag whole file (export)      {}  ({})", ms(t), if exported.is_ok() { "ok" } else { "failed" });

            if is_song {
                while shared.song_view.locked().is_none() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let view = shared.song_view.locked().clone().unwrap();
                for (n, i) in [(1, 0usize), (2, 1)] {
                    let chop = view.chops[i].clone();
                    shared.selection.locked().chop = Some(ChopSel { index: i, number: n, chop });
                    let t = Instant::now();
                    let id = shared.next_request();
                    shared.render(id, 120.0);
                    wait_rendered(&shared, id);
                    eprintln!("click chop {n}: until audible   {}", ms(t));
                }
                let t = Instant::now();
                shared.export_current().unwrap();
                eprintln!("drag current chop (export)    {}", ms(t));
                let mut sel = shared.selection.locked().clone();
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
                        *shared.stems.locked() = Some((sel.key.clone(), Arc::new(st)));
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
        *shared.selection.locked() = Selection {
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
            let state = shared.stem_state.locked().1.clone();
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
        let view = shared.song_view.locked().clone().unwrap();
        let chop = view.chops[0].clone();
        shared.stem_switches.store(0b0001, Ordering::Relaxed);
        {
            let mut sel = shared.selection.locked();
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
        shared.selection.locked().stem_mask = ALL_STEMS;
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
