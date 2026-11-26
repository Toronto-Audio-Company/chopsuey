use crate::safety::LockExt;
use nih_plug::prelude::{AsyncExecutor, Editor, GuiContext, ParentWindowHandle};
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::vizia::icons::{
    ICON_ADJUSTMENTS_ALT, ICON_FOLDER, ICON_FOLDERS, ICON_FOLDER_OPEN, ICON_FOLDER_PLUS,
    ICON_REPEAT, ICON_STAR_FILLED, ICON_SEARCH, ICON_CUT, ICON_WAVE_SINE, ICON_X, ICON_ARROW_BIG_UP,
};
use crate::licence;
use nih_plug_vizia::vizia::vg;
use nih_plug_vizia::widgets::*;
use nih_plug_vizia::{create_vizia_editor, ViziaState, ViziaTheming};
use std::any::Any;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

use crate::library::{BpmSource, BUILTIN_KEY, MAX_FILE_SECS};
use crate::stems::STEM_NAMES;
use crate::{
    dialog, drag, ChopLength, ChopSel, Selection, Shared, StemState, Task, TempoSampler, TempoSamplerParams,
    IntParam, IntRange,
    TempoScale, ALL_STEMS, SONG_MIN_SECS,
};

/// The native view (NSView* on macOS) the host opened our editor in.
/// Null while the editor is closed. Needed to start OS drags.
type ParentView = Arc<AtomicPtr<c_void>>;

const ROW_HEIGHT: f32 = 40.0;

/// GUI-side model.
///
/// vizia re-evaluates every bound lens (including `.map()` closures) once per
/// frame and redraws when the result changes, so labels bound to closures
/// that read the shared atomics stay live. (Timers don't work here: vizia's
/// plugin-window backend never ticks them.)
#[derive(Lens)]
struct AppData {
    shared: Arc<Shared>,
    #[lens(ignore)]
    params: Arc<TempoSamplerParams>,
    #[lens(ignore)]
    executor: AsyncExecutor<TempoSampler>,
    #[lens(ignore)]
    parent_view: ParentView,

    search: String,
    /// Library entries matching `search`, as display strings.
    rows: Vec<Row>,
    folders: Vec<String>,
    show_folders: bool,
    selected_key: String,
    /// Name of the selected loop or song.
    selected_info: String,
    /// "88 BPM · 1:30" (songs) or "144.81 BPM · 4 bars" (loops).
    selected_meta: String,
    /// Songs get stems and chops; loops don't.
    selected_is_song: bool,
    /// Whole-bar loop (vs a short one-shot), for the card title.
    selected_is_loop: bool,
    /// The selected sample's ½× / 1× / 2× tempo choice.
    selected_scale: f32,
    /// The selected sample is starred.
    selected_favourite: bool,
    /// The sidebar shows starred samples only.
    favourites_only: bool,
    /// Folders the user opened or closed (relative to the default: roots
    /// open, subfolders closed).
    #[lens(ignore)]
    toggled_folders: std::collections::HashSet<String>,
    /// Samples shown (matching the search), for the sidebar count.
    sample_count: usize,
    /// Feedback from the last action (export, drag, errors).
    message: String,
    /// "Trial · 9 days left", "Licensed to …": the licence line in the sidebar.
    licence_label: String,
    /// The licence key box is open.
    licence_open: bool,
    /// What's typed or pasted in the licence key box.
    licence_input: String,
    /// Why the last activation failed, or "" (shown under the box).
    licence_error: String,
    /// A key is activated (the box offers to remove it).
    licensed: bool,
    /// "Check for updates" is on.
    check_updates: bool,
    /// Index of the selected chop, -1 for the whole file.
    selected_chop: i64,
    /// Whether the last press selected something, so a drag that follows
    /// it exports that (and not a previous selection).
    #[lens(ignore)]
    press_selected: bool,
    /// Key of the sidebar row under the mouse and since when (drives the
    /// marquee). By key, not position: rows move while tempos are detected.
    #[lens(ignore)]
    hover: Option<(String, std::time::Instant)>,
    /// The row-list slot the mouse is over (see `RowList`).
    #[lens(ignore)]
    hover_slot: Option<usize>,
    /// Row-list scroll position, as vizia's ScrollView reports it (0..1 of
    /// the scrollable distance), and its visible height in logical px.
    #[lens(ignore)]
    list_scroll_y: f32,
    #[lens(ignore)]
    list_view_h: f32,
    /// Row views in the list: enough to cover its height plus one partial row.
    list_slots: usize,
    /// The list's ScrollView (known once it has reported a scroll), to move it.
    #[lens(ignore)]
    list_scroll: Option<Entity>,
    /// Row count the scroll position was last set for.
    #[lens(ignore)]
    list_len: usize,
    /// The next change to the rows should start the list at the top (a new
    /// search or filter) rather than keep the rows on screen where they are.
    #[lens(ignore)]
    list_to_top: bool,
}

#[derive(Clone, Default, Data, Lens)]
struct Row {
    /// Sample key, or the folder path for folder rows.
    key: String,
    /// A folder (collapsible) rather than a sample.
    is_folder: bool,
    /// Nesting depth, for indentation.
    depth: usize,
    /// Folder rows: open or closed.
    expanded: bool,
    /// Folder rows: number of samples inside (matching the search).
    count: String,
    name: String,
    /// "87 BPM · 2:18" under the name in the sidebar.
    meta: String,
    folder: String,
    bpm: String,
    bars: String,
    source: String,
    usable: bool,
    selected: bool,
    /// How far the name scrolls (px) when hovered, to show what's cut off;
    /// 0 when it fits.
    overflow: f32,
    /// Starred.
    favourite: bool,
}

#[derive(Clone)]
enum AppEvent {
    ToggleStem(usize),
    SetChopLength(usize),
    SelectChop(usize),
    WholeSong,
    RefreshRows,
    Search(String),
    /// Press on row-list slot n (whichever row it shows right now).
    SelectSlot(usize),
    /// The row list scrolled or changed size: scroll fraction, visible
    /// height (logical px) and the ScrollView.
    ListScrolled { y: f32, view_h: f32, scroll: Entity },
    /// The ½× / 1× / 2× tempo choice for the selected sample.
    SetTempoScale(f32),
    AddFolder,
    RemoveFolder(usize),
    ToggleFolders,
    Rescan,
    /// Press on the loop tile: play the selection (and allow a drag).
    Audition,
    /// Star / un-star the selected sample.
    ToggleFavourite,
    /// Show only starred samples in the sidebar (or everything again).
    ToggleFavouritesOnly,
    /// The mouse was let go: previews play only while held.
    StopPreview,
    /// The mouse entered / left row-list slot n (for the name marquee).
    HoverSlot(usize),
    UnhoverSlot(usize),
    ShowInFinder,
    /// Drag after pressing a row or tile (only if the press selected it).
    StartDrag,
    /// Open or close the licence key box.
    ToggleLicence,
    LicenceInput(String),
    Activate,
    Deactivate,
    /// Turn update checks on/off, or open the release page if one's found.
    UpdatesItem,
    /// Reveal the log in Finder, to send to support.
    ReportProblem,
}

impl AppData {
    /// Index of the first row in the list's viewport.
    fn first_row(&self) -> usize {
        let content = self.rows.len() as f32 * ROW_HEIGHT;
        let offset = (content - self.list_view_h).max(0.0) * self.list_scroll_y;
        (offset / ROW_HEIGHT).floor() as usize
    }

    /// The row slot `slot` shows: of the rows in view (the first
    /// `list_slots` from `first_row`), the one whose index is `slot` modulo
    /// the slot count. Scrolling by a row so only re-points the one slot
    /// that went off one edge to the row coming in at the other. None past
    /// the last row.
    fn slot_row_index(&self, slot: usize) -> Option<usize> {
        let n = self.list_slots.max(1);
        let first = self.first_row();
        let i = first + (slot + n - first % n) % n;
        (i < self.rows.len()).then_some(i)
    }

    fn slot_row(&self, slot: usize) -> Option<&Row> {
        self.slot_row_index(slot).and_then(|i| self.rows.get(i))
    }

    fn rebuild_rows(&mut self) {
        let needle = self.search.trim().to_lowercase();
        let st = self.shared.library.state.locked();
        // Search: every word must match the name, folder or key ("am",
        // "f#m"); "90-100" keeps tempos in that range. The Favourites
        // filter keeps starred samples only.
        let words: Vec<&str> = needle.split_whitespace().collect();
        let fav_only = self.favourites_only;
        let matches = |e: &crate::library::Entry| {
            if fav_only && !e.favourite {
                return false;
            }
            let key = e.musical_key.as_deref().unwrap_or("").to_lowercase();
            words.iter().all(|w| match tempo_range(w) {
                Some((lo, hi)) => e.bpm.is_some_and(|b| b >= lo && b <= hi),
                None => e.name.to_lowercase().contains(w) || e.folder.to_lowercase().contains(w) || key == *w,
            })
        };
        let sample_row = |e: &crate::library::Entry, depth: usize| Row {
            key: e.key.clone(),
            is_folder: false,
            depth,
            expanded: false,
            count: String::new(),
            name: e.name.clone(),
            meta: match (e.source, e.bpm) {
                (BpmSource::Pending, _) => "Detecting tempo…".into(),
                (_, Some(b)) => {
                    let length = match e.beats() {
                        Some(beats) if e.is_loop() => format_bars(beats),
                        _ => format_duration(e.secs),
                    };
                    match &e.musical_key {
                        Some(k) => format!("{} BPM · {k} · {length}", format_bpm(b)),
                        None => format!("{} BPM · {length}", format_bpm(b)),
                    }
                }
                (_, None) => "No tempo found".into(),
            },
            favourite: e.favourite,
            folder: e.folder.clone(),
            bpm: e.bpm.map(format_bpm).unwrap_or_else(|| "—".into()),
            bars: match e.beats() {
                Some(b) if e.is_loop() => format_bars(b),
                _ => format_duration(e.secs),
            },
            source: e.source.label().into(),
            usable: e.usable(),
            selected: e.key == self.selected_key,
            overflow: 0.0,
        };

        // Folder tree: library roots, then their subfolders, by path.
        #[derive(Default)]
        struct Node {
            children: std::collections::BTreeMap<String, Node>,
            samples: Vec<usize>,
            matching: usize,
        }
        let roots: Vec<std::path::PathBuf> = st.config.folders.clone();
        let mut tree: std::collections::BTreeMap<String, (String, Node)> = Default::default();
        let mut loose = Vec::new(); // built-in loop, or files outside every root
        let mut sample_count = 0;
        for (i, e) in st.entries.iter().enumerate() {
            if !matches(e) {
                continue;
            }
            sample_count += 1;
            let Some(path) = &e.path else {
                loose.push(i);
                continue;
            };
            let Some(root) = roots.iter().filter(|r| path.starts_with(r)).max_by_key(|r| r.as_os_str().len()) else {
                loose.push(i);
                continue;
            };
            let root_key = root.display().to_string();
            let root_name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(root_key.clone());
            let mut node: &mut Node = &mut tree.entry(root_key).or_insert_with(|| (root_name, Node::default())).1;
            node.matching += 1;
            let dirs = path.parent().and_then(|p| p.strip_prefix(root).ok()).map(|p| p.to_path_buf()).unwrap_or_default();
            for part in dirs.components() {
                let name = part.as_os_str().to_string_lossy().into_owned();
                node = node.children.entry(name).or_default();
                node.matching += 1;
            }
            node.samples.push(i);
        }

        // Flatten what's visible. Roots start open, subfolders closed; a
        // click flips that. While searching, everything with a match is open.
        let searching = !needle.is_empty();
        let mut rows: Vec<Row> = loose.iter().map(|&i| sample_row(&st.entries[i], 0)).collect();
        fn walk(
            name: &str,
            key: String,
            node: &Node,
            depth: usize,
            out: &mut Vec<Row>,
            ctx: &(&dyn Fn(usize, usize) -> Row, &std::collections::HashSet<String>, bool),
        ) {
            let (sample, toggled, searching) = ctx;
            let open = *searching || ((depth == 0) != toggled.contains(&key));
            out.push(Row {
                key: key.clone(),
                is_folder: true,
                depth,
                expanded: open,
                count: node.matching.to_string(),
                name: name.to_string(),
                meta: String::new(),
                folder: String::new(),
                bpm: String::new(),
                bars: String::new(),
                source: String::new(),
                usable: true,
                selected: false,
                overflow: 0.0,
                favourite: false,
            });
            if !open {
                return;
            }
            for (child, n) in &node.children {
                walk(child, format!("{key}/{child}"), n, depth + 1, out, ctx);
            }
            for &i in &node.samples {
                out.push(sample(i, depth + 1));
            }
        }
        let sample = |i: usize, depth: usize| sample_row(&st.entries[i], depth);
        let ctx: (&dyn Fn(usize, usize) -> Row, &std::collections::HashSet<String>, bool) =
            (&sample, &self.toggled_folders, searching);
        for (key, (name, node)) in &tree {
            walk(name, key.clone(), node, 0, &mut rows, &ctx);
        }
        for row in &mut rows {
            row.overflow = name_overflow(row);
        }
        self.rows = rows;
        self.sample_count = sample_count;
        self.folders = st.config.folders.iter().map(|p| p.display().to_string()).collect();
        if let Some(e) = st.entries.iter().find(|e| e.key == self.selected_key) {
            self.selected_info = wrap_words(&e.name, TITLE_LINE_CHARS);
            let tempo = match (e.bpm, e.source) {
                (Some(b), _) => format!("{} BPM", format_bpm(b)),
                (None, BpmSource::Pending) => "Detecting tempo…".into(),
                (None, _) => "No tempo".into(),
            };
            let length = match e.beats() {
                Some(b) if e.is_loop() => format_bars(b),
                _ => format_duration(e.secs),
            };
            self.selected_meta = match &e.musical_key {
                Some(k) => format!("{tempo} · {k} · {length}"),
                None => format!("{tempo} · {length}"),
            };
            self.selected_favourite = e.favourite;
            self.selected_is_song = !e.is_loop() && e.secs >= SONG_MIN_SECS;
            self.selected_is_loop = e.is_loop();
            self.selected_scale = e.scale;
        }
    }

    fn request_render(&self) {
        let tempo = self.shared.transport.tempo.load(Ordering::Relaxed);
        // Before the host has reported a tempo the audio thread will request
        // the first render itself.
        self.shared.want_selection();
        if tempo > 0.0 {
            let id = self.shared.next_request();
            self.executor.execute_background(Task::Render { id, bpm: tempo });
        }
    }

    /// Tells Live the set changed (see `TempoSamplerParams::picked`), so it
    /// offers to save before quitting.
    fn mark_set_changed(&self, cx: &mut EventContext) {
        set_param(cx, &self.params.picked, !self.params.picked.value());
    }

    /// Licensed or still in the trial. Otherwise says how to unlock
    /// dragging samples out (previews and pads keep working).
    fn unlocked(&mut self) -> bool {
        let status = licence::status();
        self.licence_label = licence_label(&status);
        if status.is_unlocked() {
            return true;
        }
        self.message = "Your trial has ended. Enter a licence key (bottom left) to drag samples into your set.".into();
        self.licence_open = true;
        false
    }

    /// Makes `key` the active loop and re-renders it.
    fn select_key(&mut self, key: &str) {
        self.press_selected = false;
        let Some(entry) = self.shared.library.entry(key) else { return };
        let Some(bpm) = entry.bpm else {
            self.message = format!("{}: no tempo yet", entry.name);
            return;
        };
        if entry.secs > MAX_FILE_SECS {
            let mins = MAX_FILE_SECS / 60.0;
            self.message = format!("{}: longer than {mins:.0} minutes, too long to stretch", entry.name);
            return;
        }
        *self.shared.selection.locked() = Selection {
            key: entry.key.clone(),
            path: entry.path.clone(),
            name: entry.name.clone(),
            bpm,
            chop: None,
            stem_mask: self.shared.effective_stem_mask(&entry.key),
        };
        // Songs get split into stems in the background (or loaded from cache).
        if let Some(path) = &entry.path {
            if !entry.is_loop() && entry.secs >= SONG_MIN_SECS {
                self.shared.request_stems(&entry.key, path);
            }
        }
        self.selected_key = entry.key;
        self.selected_chop = -1;
        self.message.clear();
        self.request_render();
        self.press_selected = true;
        self.rebuild_rows();
    }
}


impl Model for AppData {
    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        crate::safety::guard("ui event", || {
            event.map(|e, _| match e {
                AppEvent::ToggleStem(i) => {
                    let key = self.shared.selection.locked().key.clone();
                    if self.shared.stems_for(&key).is_none() {
                        self.message = "Stems are still being split".into();
                        return;
                    }
                    let switches = self.shared.stem_switches.load(Ordering::Relaxed) ^ (1 << i);
                    if switches == 0 {
                        self.message = "Keep at least one stem on".into();
                        return;
                    }
                    self.shared.stem_switches.store(switches, Ordering::Relaxed);
                    self.shared.selection.locked().stem_mask = switches;
                    self.message.clear();
                    self.request_render();
                    let p = &self.params;
                    let param = [&p.drums, &p.bass, &p.other, &p.vocals][*i];
                    set_param(cx, param, switches & (1 << i) != 0);
                }
                AppEvent::SetChopLength(bars) => {
                    self.shared.chop_length.store(*bars, Ordering::Relaxed);
                    set_param(cx, &self.params.chop_length, ChopLength::from_bars(*bars));
                    self.shared.refresh_pads(); // pads play the tiles shown
                    self.shared.save_session();
                }
                AppEvent::SelectChop(index) => {
                    self.press_selected = false;
                    let Some(view) = self.shared.song_view.locked().clone() else { return };
                    let Some(chop) = view.chops.get(*index).cloned() else { return };
                    let number = chop_number(&view.chops, *index);
                    {
                        let mut sel = self.shared.selection.locked();
                        if sel.key != view.key {
                            return;
                        }
                        sel.chop = Some(ChopSel { index: *index, number, chop: chop.clone() });
                    }
                    self.selected_chop = *index as i64;
                    self.message.clear();
                    self.request_render();
                    self.mark_set_changed(cx);
                    self.shared.preview.store(true, Ordering::Relaxed);
                    self.press_selected = true;
                }
                AppEvent::WholeSong => {
                    self.shared.selection.locked().chop = None;
                    self.selected_chop = -1;
                    self.message.clear();
                    self.request_render();
                    self.mark_set_changed(cx);
                    self.shared.preview.store(true, Ordering::Relaxed);
                    self.press_selected = true;
                }
                AppEvent::RefreshRows => {
                    // Development: TEMPO_SAMPLER_DEMO_SEARCH=<text> prefills the search once.
                    static DEMO_SEARCH_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                    if let Ok(text) = std::env::var("TEMPO_SAMPLER_DEMO_SEARCH") {
                        if !DEMO_SEARCH_DONE.swap(true, Ordering::Relaxed) {
                            self.search = text;
                        }
                    }
                    self.rebuild_rows();
                    // Development: TEMPO_SAMPLER_DEMO=<name> selects that entry once it appears.
                    if self.selected_key == crate::library::BUILTIN_KEY {
                        if let Some(want) = std::env::var_os("TEMPO_SAMPLER_DEMO") {
                            let want = want.to_string_lossy().to_lowercase();
                            let key = {
                                let st = self.shared.library.state.locked();
                                st.entries.iter().find(|e| e.usable() && e.name.to_lowercase().contains(&want)).map(|e| e.key.clone())
                            };
                            if let Some(key) = key {
                                self.select_key(&key);
                                self.shared.preview.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                }
                AppEvent::Search(text) => {
                    self.search = text.clone();
                    self.list_to_top = true;
                    self.rebuild_rows();
                }
                AppEvent::ListScrolled { y, view_h, scroll } => {
                    self.list_scroll_y = *y;
                    self.list_view_h = *view_h;
                    self.list_scroll = Some(*scroll);
                    let slots = (*view_h / ROW_HEIGHT).ceil() as usize + 1;
                    if slots != self.list_slots {
                        self.list_slots = slots;
                    }
                }
                AppEvent::SelectSlot(slot) => {
                    let Some(row) = self.slot_row(*slot) else { return };
                    let key = row.key.clone();
                    if row.is_folder {
                        // Open or close the folder; never a drag source.
                        self.press_selected = false;
                        if !self.search.trim().is_empty() {
                            return; // while searching, matching folders stay open
                        }
                        if !self.toggled_folders.remove(&key) {
                            self.toggled_folders.insert(key);
                        }
                        self.rebuild_rows();
                    } else {
                        self.select_key(&key);
                        self.mark_set_changed(cx);
                        // Held down: audition it, like a sample browser.
                        if self.press_selected {
                            self.shared.preview.store(true, Ordering::Relaxed);
                        }
                        // The tempo parameter shows this file's ½× / 1× / 2×.
                        if let Some(e) = self.shared.library.entry(&key) {
                            let scale = TempoScale::from_factor(e.scale);
                            if self.params.tempo_scale.value() != scale {
                                set_param(cx, &self.params.tempo_scale, scale);
                            }
                        }
                    }
                }
                AppEvent::SetTempoScale(scale) => {
                    let key = self.selected_key.clone();
                    if key == BUILTIN_KEY {
                        self.message = "The built-in loop's tempo is fixed".into();
                        return;
                    }
                    if self.shared.library.entry(&key).is_some_and(|e| e.base_bpm.is_some() && e.scale != *scale) {
                        self.shared.library.set_scale(&key, *scale);
                        self.select_key(&key);
                        set_param(cx, &self.params.tempo_scale, TempoScale::from_factor(*scale));
                    }
                }
                AppEvent::AddFolder => {
                    // The picker returns immediately; folders arrive in the callback.
                    let library = self.shared.library.clone();
                    match dialog::pick_folders(move |folders| library.add_folders(folders)) {
                        Ok(()) => self.show_folders = true,
                        Err(e) => self.message = e,
                    }
                }
                AppEvent::RemoveFolder(index) => self.shared.library.remove_folder(*index),
                AppEvent::ToggleFolders => self.show_folders = !self.show_folders,
                AppEvent::Rescan => self.shared.library.rescan(),
                AppEvent::ToggleFavourite => {
                    if self.selected_key != BUILTIN_KEY {
                        self.shared.library.toggle_favourite(&self.selected_key);
                    }
                }
                AppEvent::ToggleFavouritesOnly => {
                    self.favourites_only = !self.favourites_only;
                    self.list_to_top = true;
                    self.rebuild_rows();
                }
                AppEvent::StopPreview => {
                    self.shared.preview.store(false, Ordering::Relaxed);
                }
                AppEvent::HoverSlot(slot) => {
                    if let Some(row) = self.slot_row(*slot) {
                        self.hover = Some((row.key.clone(), std::time::Instant::now()));
                        self.hover_slot = Some(*slot);
                    }
                }
                AppEvent::UnhoverSlot(slot) => {
                    // Leaving one row can arrive after entering the next.
                    if self.hover_slot == Some(*slot) {
                        self.hover = None;
                        self.hover_slot = None;
                    }
                }
                AppEvent::Audition => {
                    self.message.clear();
                    self.shared.preview.store(true, Ordering::Relaxed);
                    self.press_selected = true;
                }
                AppEvent::StartDrag => {
                    // The OS takes the mouse from here, so the release never arrives.
                    self.shared.preview.store(false, Ordering::Relaxed);
                    if !self.press_selected {
                        return; // the press didn't select anything draggable
                    }
                    if !self.unlocked() {
                        return;
                    }
                    let result = self.shared.export_current().and_then(|path| {
                        drag::start_file_drag(self.parent_view.load(Ordering::Acquire), &path)
                    });
                    if let Err(e) = result {
                        self.message = format!("Drag failed: {e}");
                    }
                }
                AppEvent::ShowInFinder if !self.unlocked() => {}
                AppEvent::ShowInFinder => {
                    self.message = match self.shared.export_current() {
                        Ok(path) => {
                            let _ = std::process::Command::new("open").arg("-R").arg(&path).spawn();
                            format!("Saved {}", path.file_name().unwrap_or_default().to_string_lossy())
                        }
                        Err(e) => format!("Export failed: {e}"),
                    };
                }
                AppEvent::ToggleLicence => {
                    self.licence_open = !self.licence_open;
                    self.licence_error.clear();
                }
                AppEvent::LicenceInput(text) => self.licence_input = text.clone(),
                AppEvent::Activate => match licence::activate(&self.licence_input) {
                    Ok(status) => {
                        self.licence_label = licence_label(&status);
                        self.licensed = true;
                        self.licence_input.clear();
                        self.licence_error.clear();
                        self.licence_open = false;
                        self.message = "Thanks! chopsuey is activated.".into();
                    }
                    Err(e) => self.licence_error = e,
                },
                AppEvent::Deactivate => {
                    licence::deactivate();
                    let status = licence::status();
                    self.licence_label = licence_label(&status);
                    self.licensed = false;
                }
                AppEvent::UpdatesItem => {
                    if let Some(release) = crate::update::available() {
                        let _ = std::process::Command::new("open").arg(&release.url).spawn();
                    } else {
                        self.check_updates = !self.check_updates;
                        self.shared.library.set_check_updates(self.check_updates);
                        if self.check_updates {
                            crate::update::check_in_background();
                        } else {
                            crate::update::disable();
                        }
                    }
                }
                AppEvent::ReportProblem => {
                    self.message = match crate::log::path().filter(|p| p.exists()) {
                        Some(path) => {
                            let _ = std::process::Command::new("open").arg("-R").arg(&path).spawn();
                            "The log is selected in Finder: attach it to an email to support, with what you were doing.".into()
                        }
                        None => "Nothing has been logged yet.".into(),
                    };
                }
            });
            self.keep_list_position(cx);
        });
    }
}

impl AppData {
    /// vizia's ScrollView remembers its position as a fraction of the
    /// scrollable distance, so when rows come or go (a folder opens, a scan
    /// adds files) the list would slide. Keep the rows on screen where they
    /// were instead, or go back to the top for a new search or filter.
    fn keep_list_position(&mut self, cx: &mut EventContext) {
        let len = self.rows.len();
        if len == self.list_len && !self.list_to_top {
            return;
        }
        let span = |n: usize| (n as f32 * ROW_HEIGHT - self.list_view_h).max(0.0);
        let offset = span(self.list_len) * self.list_scroll_y;
        let y = if self.list_to_top || span(len) == 0.0 { 0.0 } else { (offset / span(len)).min(1.0) };
        self.list_len = len;
        self.list_to_top = false;
        if y != self.list_scroll_y {
            self.list_scroll_y = y;
            if let Some(scroll) = self.list_scroll {
                cx.emit_to(scroll, ScrollEvent::SetY(y));
            }
        }
    }
}

pub(crate) fn default_state() -> Arc<ViziaState> {
    ViziaState::new(|| (1120, 784))
}

fn format_bpm(bpm: f32) -> String {
    let s = format!("{bpm:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn format_bars(beats: f32) -> String {
    let bars = beats / 4.0;
    if bars == 1.0 {
        "1 bar".to_string()
    } else if bars.fract() == 0.0 {
        format!("{bars} bars")
    } else {
        format!("{beats} beats")
    }
}

/// About how many characters of the 32px bold title fit on a line at the
/// default window width.
const TITLE_LINE_CHARS: usize = 34;

/// Breaks `text` between words into lines of at most `max` characters (a
/// longer word gets a line of its own). vizia only wraps a label at a fixed
/// width, and the title is sized to its text so the button can sit beside it.
fn wrap_words(text: &str, max: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= max => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines.join("\n")
}

fn format_duration(secs: f32) -> String {
    if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        let s = secs.round() as u32;
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Something worth saying about the preview: an error, or that it's still
/// stretching. Nothing once it's playing.
fn render_status(s: &Shared) -> Option<String> {
    if s.audio_fault.load(Ordering::Relaxed) {
        return Some("Sound stopped after an internal error. Reopen the set (or remove and re-add chopsuey) to start it again; Report a problem has the details.".into());
    }
    if let Some(e) = s.render_error.locked().as_ref() {
        return Some(e.clone());
    }
    let ready = s.current().is_some() && s.is_up_to_date() && s.rendered_bpm.load(Ordering::Relaxed) > 0.0;
    (!ready).then(|| "Stretching…".to_string())
}

/// Shown under the list while scanning. Reads atomics only (runs every frame).
fn library_summary(s: &Shared) -> String {
    match s.library.detecting.load(Ordering::Relaxed) {
        0 => "Scanning…".into(),
        n => format!("Detecting tempo · {n} left"),
    }
}

pub(crate) fn create(
    shared: Arc<Shared>,
    executor: AsyncExecutor<TempoSampler>,
    params: Arc<TempoSamplerParams>,
    editor_state: Arc<ViziaState>,
) -> Option<Box<dyn Editor>> {
    shared.library.ensure_loaded();

    let parent_view: ParentView = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
    let gui_parent_view = parent_view.clone();
    let close_shared = shared.clone();

    let inner = create_vizia_editor(editor_state, ViziaTheming::Custom, move |cx, _| {
        let t0 = std::time::Instant::now();
        // Inter (SIL Open Font License, assets/fonts/Inter-LICENSE.txt).
        // Static files: this vizia can't pick weights from the variable font.
        cx.add_font_mem(include_bytes!("../assets/fonts/Inter-Regular.ttf"));
        cx.add_font_mem(include_bytes!("../assets/fonts/Inter-Bold.ttf"));
        // All colours, spacing and type live in the stylesheet (src/style.css).
        if let Err(e) = cx.add_stylesheet(include_str!("style.css")) {
            nih_plug::nih_log!("chopsuey: couldn't load stylesheet: {e}");
        }

        // Development: TEMPO_SAMPLER_DEMO_STEMS=0011 sets the stem switches.
        if let Some(bits) = std::env::var("TEMPO_SAMPLER_DEMO_STEMS").ok().and_then(|b| u8::from_str_radix(&b, 2).ok()) {
            shared.stem_switches.store(bits, Ordering::Relaxed);
        }
        // Development: TEMPO_SAMPLER_DEMO_LENGTH=<bars> (0: one shots) picks the chop grid.
        if let Some(bars) = std::env::var("TEMPO_SAMPLER_DEMO_LENGTH").ok().and_then(|b| b.parse::<usize>().ok()) {
            shared.chop_length.store(bars, Ordering::Relaxed);
        }
        let selected_key = shared.selection.locked().key.clone();
        let check_updates = shared.library.check_updates();
        if check_updates {
            crate::update::check_in_background();
        }
        AppData {
            shared: shared.clone(),
            params: params.clone(),
            executor: executor.clone(),
            parent_view: gui_parent_view.clone(),
            search: String::new(),
            rows: Vec::new(),
            folders: Vec::new(),
            show_folders: false,
            selected_key,
            selected_info: String::new(),
            selected_meta: String::new(),
            selected_is_song: false,
            selected_is_loop: false,
            selected_scale: 1.0,
            selected_favourite: false,
            favourites_only: false,
            // Development: TEMPO_SAMPLER_DEMO_HOVER=<key> starts as if that
            // sidebar row were hovered (to see the marquee in screenshots).
            hover: std::env::var("TEMPO_SAMPLER_DEMO_HOVER")
                .ok()
                .map(|key| (key, std::time::Instant::now())),
            hover_slot: None,
            list_scroll_y: 0.0,
            list_view_h: 0.0,
            list_slots: 0,
            list_scroll: None,
            list_len: 0,
            list_to_top: false,
            toggled_folders: Default::default(),
            sample_count: 0,
            message: String::new(),
            licence_label: licence_label(&licence::status()),
            licence_open: false,
            licence_input: String::new(),
            licence_error: String::new(),
            licensed: matches!(licence::status(), licence::Status::Licensed { .. }),
            check_updates,
            // A chop restored from the Live set shows as selected.
            selected_chop: shared.selection.locked().chop.as_ref().map_or(-1, |c| c.index as i64),
            press_selected: true,
        }
        .build(cx);

        // Rebuild the list whenever the library changes (scan, detection, edits).
        Binding::new(
            cx,
            AppData::shared.map(|s| s.library.version.load(Ordering::Relaxed)),
            |cx, _| cx.emit(AppEvent::RefreshRows),
        );

        HStack::new(cx, |cx| {
            sidebar(cx);
            sheet(cx);
        })
        // A thin dark frame around the sheet.
        .child_space(Pixels(4.0))
        .child_left(Pixels(0.0))
        .col_between(Pixels(4.0));

        ResizeHandle::new(cx);
        if std::env::var_os("TEMPO_SAMPLER_TIMING").is_some() {
            crate::logln!("editor: built in {:?}", t0.elapsed());
        }
    })?;

    Some(Box::new(ParentViewEditor {
        inner,
        parent_view,
        shared: close_shared,
    }))
}

/// Vertically centres a row's children.
fn centred<V: View>(h: Handle<'_, V>) -> Handle<'_, V> {
    h.child_top(Stretch(1.0)).child_bottom(Stretch(1.0))
}

/// A sidebar action: icon and label, styled like a navigation item.
fn nav_item<'a, T: ToStringLocalized + 'static>(
    cx: &'a mut Context,
    icon: &'static str,
    label: impl Res<T> + Clone + 'static,
    event: impl Fn() -> AppEvent + Send + Sync + 'static,
) -> Handle<'a, Button> {
    Button::new(cx, move |cx| cx.emit(event()), move |cx| {
        centred(HStack::new(cx, move |cx| {
            Icon::new(cx, icon);
            Label::new(cx, label.clone());
        }))
        .col_between(Pixels(ICON_GAP))
        .width(Auto)
    })
    .class("nav-item")
    .child_left(Pixels(ICON_COLUMN))
    .width(Stretch(1.0))
}

// ============================ Sidebar ============================
//
// Sidebar lines (logical px from the sidebar's content edge, which is also
// the box line: search box, row highlights, logo):
//   icon column  10..26 from a row's edge (16px icons)
//   text line    34 from a row's edge: labels, placeholder, app name
//   each nesting level moves both by one column (icon + gap), so a child's
//   icon sits where its parent's label starts.
// Counts end on the trailing line, 6px in from a row's right edge.

const ICON_COLUMN: f32 = 8.0;
const ICON_SIZE: f32 = 16.0;
const ICON_GAP: f32 = 8.0;
const TEXT_LINE: f32 = ICON_COLUMN + ICON_SIZE + ICON_GAP;
const INDENT: f32 = ICON_SIZE + ICON_GAP;
const BRAND_MARK: f32 = 24.0;

/// Bar length shown, from the shared value (the GUI, automation or a
/// restored set can change it).
fn chop_length_lens() -> impl Lens<Target = usize> {
    AppData::shared.map(|s| s.chop_length.load(Ordering::Relaxed))
}

/// A search word like "90-100" (or "90–100"): the tempo range it means.
fn tempo_range(word: &str) -> Option<(f32, f32)> {
    let (a, b) = word.split_once(['-', '–'])?;
    let (a, b): (f32, f32) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    Some((a.min(b), a.max(b)))
}

/// Sets a host parameter from the GUI as one gesture, so Live records it
/// (automation lanes follow clicks).
fn set_param<P: nih_plug::prelude::Param>(cx: &mut EventContext, param: &P, value: P::Plain) {
    use nih_plug_vizia::widgets::ParamEvent;
    cx.emit(ParamEvent::BeginSetParameter(param).upcast());
    cx.emit(ParamEvent::SetParameter(param, value).upcast());
    cx.emit(ParamEvent::EndSetParameter(param).upcast());
}

/// A sidebar row's name, clipped to the row. While the row is hovered, a
/// name that doesn't fit scrolls to show the rest, car-stereo style.
fn marquee_name<L: Lens<Target = String>>(cx: &mut Context, name: L, slot: usize) {
    HStack::new(cx, |cx| {
        Label::new(cx, name)
            .class("name")
            .width(Auto)
            .text_wrap(false)
            .hoverable(false)
            // Translation moves only the drawing, not the layout.
            .translate(AppData::root.map(move |d| Translate::new(Length::px(-marquee_x(d, slot)), Length::px(0.0))));
        // While scrolled, the start is cut off too: fade that edge like the
        // end. Shown only while the name is scrolled away from its start.
        Element::new(cx)
            .class("fade-left")
            .position_type(PositionType::SelfDirected)
            .left(Pixels(0.0))
            .top(Pixels(0.0))
            .width(Pixels(24.0))
            .height(Percentage(100.0))
            .toggle_class("hidden", AppData::root.map(move |d| marquee_x(d, slot) < 1.0))
            .hoverable(false);
    })
    .width(Stretch(1.0))
    .min_width(Pixels(0.0))
    .height(Auto)
    .overflow(Overflow::Hidden)
    .hoverable(false);
}

/// How far the name in row-list slot `slot` is scrolled right now (0
/// unless hovered and too long to fit).
fn marquee_x(d: &AppData, slot: usize) -> f32 {
    match (&d.hover, d.slot_row(slot)) {
        (Some((key, since)), Some(row)) if *key == row.key => marquee_offset(row.overflow, since.elapsed().as_secs_f32()),
        _ => 0.0,
    }
}

/// Marquee position after `t` seconds of hover for a name `dist` px too
/// long: wait, scroll to the end, hold, scroll back, repeat.
fn marquee_offset(dist: f32, t: f32) -> f32 {
    const WAIT: f32 = 0.8;
    const HOLD: f32 = 1.2;
    const SPEED: f32 = 30.0; // px per second
    if dist <= 0.0 {
        return 0.0;
    }
    let travel = dist / SPEED;
    let t = t % (WAIT + travel + HOLD + travel);
    let x = if t < WAIT {
        0.0
    } else if t < WAIT + travel {
        (t - WAIT) * SPEED
    } else if t < WAIT + travel + HOLD {
        dist
    } else {
        dist - (t - WAIT - travel - HOLD) * SPEED
    };
    (x * 2.0).round() / 2.0 // half-pixel steps: fewer redraws, still smooth
}

/// Width of `text` in Inter at `size` px (sum of glyph advances).
fn text_width(text: &str, size: f32, bold: bool) -> f32 {
    static FACES: std::sync::OnceLock<(ttf_parser::Face<'static>, ttf_parser::Face<'static>)> = std::sync::OnceLock::new();
    let (regular, heavy) = FACES.get_or_init(|| {
        (
            ttf_parser::Face::parse(include_bytes!("../assets/fonts/Inter-Regular.ttf"), 0).expect("Inter"),
            ttf_parser::Face::parse(include_bytes!("../assets/fonts/Inter-Bold.ttf"), 0).expect("Inter"),
        )
    });
    let face = if bold { heavy } else { regular };
    let units: u32 = text
        .chars()
        .map(|c| face.glyph_index(c).and_then(|g| face.glyph_hor_advance(g)).unwrap_or(0) as u32)
        .sum();
    units as f32 * size / face.units_per_em() as f32
}

/// How far a row's name must scroll to show its end clear of the fade
/// (0 if it fits). Widths follow the row layout in `sidebar`.
fn name_overflow(row: &Row) -> f32 {
    const LIST: f32 = 288.0; // style.css `.row-list ... vstack`
    const FADE: f32 = 24.0;
    let depth = row.depth as f32 * INDENT;
    let room = if row.is_folder {
        let count = text_width(&row.count, 11.0, false);
        LIST - (ICON_COLUMN + depth) - ICON_SIZE - ICON_GAP * 3.0 - count - ICON_COLUMN
    } else {
        LIST - (TEXT_LINE + depth) - ICON_COLUMN
    };
    let text = text_width(&row.name, 13.0, row.is_folder);
    if text > room { text - room + FADE } else { 0.0 }
}

/// A short gradient laid over the end of the text before it, so text cut
/// off by its container fades out instead of stopping mid-letter. It goes
/// right after the label in a row and overlaps its last 24px; its colour
/// follows the row's background (style.css `.fade`).
fn fade(cx: &mut Context) {
    Element::new(cx)
        .class("fade")
        .width(Pixels(24.0))
        .left(Pixels(-24.0))
        .height(Stretch(1.0))
        .hoverable(false);
}

/// Lens to the row that row-list slot `n` shows (see
/// `AppData::slot_row_index`), by reference so bindings clone nothing
/// unless it changed. Slots past the last row see an empty row (and are
/// hidden).
#[derive(Clone, Copy, Debug)]
struct SlotRow(usize);

impl std::hash::Hash for SlotRow {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Bindings share a store per lens hash: keep ours distinct.
        std::any::TypeId::of::<Self>().hash(state);
        self.0.hash(state);
    }
}

impl Lens for SlotRow {
    type Source = AppData;
    type Target = Row;

    fn view<O, F: FnOnce(Option<&Row>) -> O>(&self, data: &AppData, map: F) -> O {
        static EMPTY: std::sync::OnceLock<Row> = std::sync::OnceLock::new();
        map(Some(data.slot_row(self.0).unwrap_or_else(|| EMPTY.get_or_init(Row::default))))
    }
}

/// Row view for row-list slot `slot`: shows whichever row
/// `AppData::slot_row_index` puts in that slot, at that row's position.
fn list_row(cx: &mut Context, slot: usize) {
    let row = SlotRow(slot);
    // Folders: press to open/close. Samples: press to select and
    // audition, press and drag to drag it out.
    let indent = row.then(Row::depth).map(|d| Pixels(ICON_COLUMN + *d as f32 * INDENT));
    PressDrag::new(cx, AppEvent::SelectSlot(slot), move |cx| {
        // Folder variant
        // The icon opens and closes with the folder (no separate
        // chevron), so the icon column and text line stay clear.
        centred(HStack::new(cx, |cx| {
            Icon::new(
                cx,
                row.then(Row::expanded).map(|e| if *e { ICON_FOLDER_OPEN } else { ICON_FOLDER }),
            )
            .hoverable(false);
            marquee_name(cx, row.then(Row::name), slot);
            fade(cx);
            Label::new(cx, row.then(Row::count)).class("folder-count").hoverable(false);
        }))
        .class("row-variant")
        .col_between(Pixels(ICON_GAP))
        .child_left(indent)
        .child_right(Pixels(ICON_COLUMN))
        .toggle_class("hidden", row.then(Row::is_folder).map(|f| !*f))
        .hoverable(false);

        // Sample variant
        // Labels fill the row's width (not their text's) so long
        // names are clipped inside the highlight, under a fade.
        HStack::new(cx, |cx| {
            VStack::new(cx, |cx| {
                marquee_name(cx, row.then(Row::name), slot);
                HStack::new(cx, |cx| {
                    Icon::new(cx, ICON_STAR_FILLED)
                        .class("row-star")
                        .toggle_class("hidden", row.then(Row::favourite).map(|f| !*f))
                        .hoverable(false);
                    Label::new(cx, row.then(Row::meta))
                        .class("meta")
                        .width(Stretch(1.0))
                        .min_width(Pixels(0.0))
                        .text_wrap(false)
                        .overflow(Overflow::Hidden)
                        .hoverable(false);
                })
                .col_between(Pixels(4.0))
                .height(Auto)
                .hoverable(false);
            })
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0))
            .row_between(Pixels(0.0))
            .hoverable(false);
            fade(cx);
        })
        .class("row-variant")
        // No icon: the label starts on this level's text line.
        .child_left(row.then(Row::depth).map(|d| Pixels(TEXT_LINE + *d as f32 * INDENT)))
        .child_right(Pixels(ICON_COLUMN))
        .toggle_class("hidden", row.then(Row::is_folder))
        .hoverable(false);
    })
    .class("row")
    .width(Percentage(100.0))
    .toggle_class("folder", row.then(Row::is_folder))
    .checked(row.then(Row::selected))
    .toggle_class("muted", row.then(Row::usable).map(|u| !*u))
    .position_type(PositionType::SelfDirected)
    .top(AppData::root.map(move |d| Pixels(d.slot_row_index(slot).unwrap_or(0) as f32 * ROW_HEIGHT)))
    .height(Pixels(ROW_HEIGHT))
    .toggle_class("hidden", AppData::root.map(move |d| d.slot_row_index(slot).is_none()));
}

fn sidebar(cx: &mut Context) {
    VStack::new(cx, |cx| {
        // Brand
        centred(HStack::new(cx, |cx| {
            // The takeout box (assets/logo.svg) on a white tile.
            HStack::new(cx, |cx| {
                crate::logo::Logo::new(cx).size(Stretch(1.0));
            })
            .class("brand-mark")
            .child_space(Pixels(2.0));
            Label::new(cx, crate::library::APP_NAME);
        }))
        .class("brand")
        // Logo (22px, wider than the icon column) on the box line, name on
        // the text line.
        .col_between(Pixels(TEXT_LINE - BRAND_MARK))
        .child_left(Pixels(0.0))
        .height(Pixels(32.0));

        // Search: icon on the icon column, text on the text line (the box's
        // 1px border counts).
        centred(HStack::new(cx, |cx| {
            Icon::new(cx, ICON_SEARCH);
            Textbox::new(cx, AppData::search)
                .placeholder("Search")
                .on_edit(|cx, text| cx.emit(AppEvent::Search(text)))
                .width(Stretch(1.0));
        }))
        .class("search")
        .child_left(Pixels(ICON_COLUMN - 1.0))
        .col_between(Pixels(ICON_GAP));

        // "Library  225"
        centred(HStack::new(cx, |cx| {
            Label::new(cx, "Library").width(Stretch(1.0));
            Label::new(cx, AppData::sample_count.map(|n| n.to_string())).class("count");
        }))
        .class("section-label")
        .child_left(Pixels(ICON_COLUMN))
        .child_right(Pixels(ICON_COLUMN))
        .height(Pixels(24.0));

        // The library list. vizia's VirtualList rebuilds every visible row
        // each time the list scrolls past a row boundary, which made
        // scrolling stutter; this keeps a fixed set of row views and only
        // re-points the one that scrolls off an edge (see `slot_row_index`).
        ScrollView::new(cx, 0.0, 0.0, false, true, |cx| {
            VStack::new(cx, |cx| {
                Binding::new(cx, AppData::list_slots, |cx, slots| {
                    for slot in 0..slots.get(cx) {
                        list_row(cx, slot);
                    }
                });
            })
            .height(AppData::rows.map(|rows| Pixels(rows.len() as f32 * ROW_HEIGHT)));
        })
        .class("row-list")
        .scroll_to_cursor(true)
        .on_scroll(|cx, _, y| {
            let view_h = cx.bounds().h / cx.scale_factor();
            let scroll = cx.current();
            cx.emit(AppEvent::ListScrolled { y, view_h, scroll });
        })
        .height(Stretch(1.0));

        // Scan status, only while scanning.
        Label::new(cx, AppData::shared.map(|s| library_summary(s)))
            .class("section-label")
            .class("muted")
            .child_left(Pixels(ICON_COLUMN))
            .toggle_class(
                "hidden",
                AppData::shared.map(|s| !s.library.busy.load(Ordering::Relaxed)),
            );

        Element::new(cx).height(Pixels(1.0)).background_color(Color::rgb(0x22, 0x22, 0x22));
        VStack::new(cx, |cx| {
            nav_item(cx, ICON_STAR_FILLED, "Favourites", || AppEvent::ToggleFavouritesOnly)
                .checked(AppData::favourites_only);
            nav_item(cx, ICON_FOLDER_PLUS, "Add folder", || AppEvent::AddFolder);
            nav_item(
                cx,
                ICON_FOLDERS,
                "Folders",
                || AppEvent::ToggleFolders,
            );
            folder_list(cx);
            nav_item(cx, ICON_REPEAT, "Rescan", || AppEvent::Rescan);
        })
        .class("nav-group");

        Element::new(cx).height(Pixels(1.0)).background_color(Color::rgb(0x22, 0x22, 0x22));
        VStack::new(cx, |cx| {
            nav_item(cx, ICON_KEY, AppData::licence_label, || AppEvent::ToggleLicence).checked(AppData::licence_open);
            licence_box(cx);
            nav_item(
                cx,
                ICON_ARROW_BIG_UP,
                AppData::shared.map(|_| update_label()),
                || AppEvent::UpdatesItem,
            )
            .checked(AppData::check_updates);
            nav_item(cx, ICON_BUG, "Report a problem", || AppEvent::ReportProblem);
        })
        .class("nav-group");
    })
    .class("sidebar")
    .height(Stretch(1.0));
}

/// The licence key box under the licence line, while it's open.
fn licence_box(cx: &mut Context) {
    VStack::new(cx, |cx| {
        Textbox::new(cx, AppData::licence_input)
            .placeholder("Paste your licence key")
            .on_edit(|cx, text| cx.emit(AppEvent::LicenceInput(text)))
            .on_submit(|cx, _, _| cx.emit(AppEvent::Activate))
            .class("search")
            .width(Stretch(1.0));
        Label::new(cx, AppData::licence_error)
            .class("muted")
            .text_wrap(true)
            .toggle_class("hidden", AppData::licence_error.map(|e| e.is_empty()));
        HStack::new(cx, |cx| {
            Button::new(cx, |cx| cx.emit(AppEvent::Activate), |cx| Label::new(cx, "Activate")).class("nav-item");
            Button::new(cx, |cx| cx.emit(AppEvent::Deactivate), |cx| Label::new(cx, "Remove licence"))
                .class("nav-item")
                .toggle_class("hidden", AppData::licensed.map(|l| !l));
        })
        .col_between(Pixels(ICON_GAP))
        .height(Auto);
    })
    .child_left(Pixels(ICON_COLUMN))
    .row_between(Pixels(4.0))
    .height(Auto)
    .toggle_class("hidden", AppData::licence_open.map(|open| !open));
}

/// The licence line: where the trial or licence stands.
fn licence_label(status: &licence::Status) -> String {
    match status {
        licence::Status::Licensed { name, .. } => format!("Licensed to {name}"),
        licence::Status::Trial { days_left: 1 } => "Trial · 1 day left".into(),
        licence::Status::Trial { days_left } => format!("Trial · {days_left} days left"),
        licence::Status::TrialOver => "Trial over · enter a key".into(),
    }
}

/// The updates line. Runs every frame (a found release appears without a
/// click); the lock it reads is uncontended.
fn update_label() -> String {
    match crate::update::available() {
        Some(release) => format!("Update {} available", release.version),
        None if crate::update::ENABLED.load(Ordering::Relaxed) => "Checking for updates".into(),
        None => "Check for updates".into(),
    }
}

fn folder_list(cx: &mut Context) {
    Binding::new(cx, AppData::show_folders, |cx, show| {
        if !show.get(cx) {
            return;
        }
        VStack::new(cx, |cx| {
            Binding::new(cx, AppData::folders.map(|f| f.is_empty()), |cx, empty| {
                if empty.get(cx) {
                    Label::new(cx, "No folders yet");
                }
            });
            List::new(cx, AppData::folders, |cx, index, folder| {
                centred(HStack::new(cx, |cx| {
                    Label::new(cx, folder)
                        .width(Stretch(1.0))
                        .text_wrap(false)
                        .overflow(Overflow::Hidden);
                    fade(cx);
                    Button::new(cx, move |cx| cx.emit(AppEvent::RemoveFolder(index)), |cx| Icon::new(cx, ICON_X))
                        .class("nav-item")
                        .width(Pixels(24.0))
                        .height(Pixels(24.0));
                }))
                .height(Pixels(24.0));
            })
            .height(Auto);
        })
        .class("folder-list")
        .height(Auto);
    });
}

// ============================ Sheet ============================
//
// Sheet lines: cards and controls sit on the box lines (the sheet's
// padding); text (title, details) sits on the cards' inner text
// lines, CARD_INSET further in, so the title and card contents share an edge.

/// A card's padding (style.css `.card`: 24 radius, 24 inset, no border).
const CARD_INSET: f32 = 24.0;

fn sheet(cx: &mut Context) {
    VStack::new(cx, |cx| {
        title_block(cx);
        song_cards(cx);
        Element::new(cx).height(Stretch(1.0));
        status_line(cx);
    })
    .class("sheet")
    .width(Stretch(1.0))
    .height(Stretch(1.0));
}

/// Title (with a small Show in Finder button beside it) and details, on the
/// text line.
fn title_block(cx: &mut Context) {
    VStack::new(cx, |cx| {
        // Top-aligned so the button stays level with the first line when
        // the title wraps: its centre sits on the first line's cap-height
        // centre (measured: 5px down).
        HStack::new(cx, |cx| {
            Label::new(cx, AppData::selected_info)
                .class("page-title")
                .text_wrap(false)
                .overflow(Overflow::Hidden)
                .width(Auto)
                .max_width(Percentage(92.0));
            show_in_finder(cx);
            Button::new(cx, |cx| cx.emit(AppEvent::ToggleFavourite), |cx| {
                Icon::new(cx, AppData::selected_favourite.map(|f| if *f { ICON_STAR_FILLED } else { ICON_STAR }))
            })
            .class("icon-button")
            .toggle_class("starred", AppData::selected_favourite)
            .top(Pixels(5.0));
        })
        .col_between(Pixels(8.0))
        .child_top(Pixels(0.0))
        .height(Auto);
        // "88 BPM · 1:30", plus "Stretching…" or an error while relevant;
        // then the controls that apply to the whole file: pitch and tempo.
        HStack::new(cx, |cx| {
            Label::new(
                cx,
                AppData::root.map(|d| match render_status(&d.shared) {
                    Some(status) => format!("{} · {status}", d.selected_meta),
                    None => d.selected_meta.clone(),
                }),
            )
            .class("subdued")
            .text_wrap(false)
            .width(Stretch(1.0));
            pitch_controls(cx);
            tempo_switch(cx);
        })
        .class("title-controls")
        .col_between(Pixels(8.0))
        .child_top(Stretch(1.0))
        .child_bottom(Stretch(1.0))
        .height(Pixels(32.0));
    })
    .row_between(Pixels(8.0))
    .child_left(Pixels(CARD_INSET))
    .child_right(Pixels(CARD_INSET))
    .height(Auto);
}

/// ½× / 1× / 2×: the detected tempo, halved or doubled when detection
/// picked the wrong octave. Sits beside the details under the title.
fn tempo_switch(cx: &mut Context) {
    HStack::new(cx, |cx| {
        for (scale, label) in [(0.5f32, "½×"), (1.0, "1×"), (2.0, "2×")] {
            Button::new(cx, move |cx| cx.emit(AppEvent::SetTempoScale(scale)), move |cx| Label::new(cx, label))
                .checked(AppData::selected_scale.map(move |s| *s == scale));
        }
    })
    .class("segmented");
}

/// Transpose and Detune, like the boxes in Ableton's clip view. They pitch
/// everything (previews, pads, dragged files) without changing the tempo.
fn pitch_controls(cx: &mut Context) {
    HStack::new(cx, |cx| {
        PitchBox::new(cx, |p| &p.transpose, "st", 4.0);
        PitchBox::new(cx, |p| &p.detune, "ct", 2.0);
    })
    .class("segmented")
    .class("pitch");
}

/// A value box for an integer parameter: drag up or down to change it
/// (`px_per_step` pixels a step), scroll to step it, double-click to reset
/// it to 0. One drag is one host gesture, so Live records it as one change.
struct PitchBox {
    params: Arc<TempoSamplerParams>,
    param: fn(&TempoSamplerParams) -> &IntParam,
    px_per_step: f32,
    /// While dragging: the pointer's y and the value where the drag started.
    drag: Option<(f32, i32)>,
    /// Scroll not yet turned into steps (trackpads scroll in fractions).
    scroll: f32,
}

impl PitchBox {
    fn new<'a>(
        cx: &'a mut Context,
        param: fn(&TempoSamplerParams) -> &IntParam,
        unit: &'static str,
        px_per_step: f32,
    ) -> Handle<'a, Self> {
        let params = AppData::root.map(|d| d.params.clone()).get(cx);
        let value = AppData::root.map(move |d| param(&d.params).value());
        Self { params, param, px_per_step, drag: None, scroll: 0.0 }
            .build(cx, move |cx| {
                Label::new(cx, value.map(move |v| format!("{} {unit}", crate::signed(*v)))).hoverable(false);
            })
            .cursor(CursorIcon::NsResize)
            .toggle_class("shifted", value.map(|v| *v != 0))
    }

    fn set(&self, cx: &mut EventContext, value: i32) {
        use nih_plug_vizia::widgets::ParamEvent;
        let param = (self.param)(&self.params);
        let gesture = self.drag.is_none();
        if gesture {
            cx.emit(ParamEvent::BeginSetParameter(param).upcast());
        }
        cx.emit(ParamEvent::SetParameter(param, value).upcast());
        if gesture {
            cx.emit(ParamEvent::EndSetParameter(param).upcast());
        }
    }
}

impl View for PitchBox {
    fn element(&self) -> Option<&'static str> {
        Some("pitch-box")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        crate::safety::guard("ui event", || {
            use nih_plug_vizia::widgets::ParamEvent;
            let params = self.params.clone();
            let param = (self.param)(&params);
            let (min, max) = match param.range() {
                IntRange::Linear { min, max } => (min, max),
                _ => (i32::MIN, i32::MAX),
            };
            event.map(|window_event, meta| match window_event {
                WindowEvent::MouseDown(MouseButton::Left) => {
                    cx.emit(ParamEvent::BeginSetParameter(param).upcast());
                    self.drag = Some((cx.mouse().cursory, param.value()));
                    cx.capture();
                    meta.consume();
                }
                WindowEvent::MouseDoubleClick(MouseButton::Left) => {
                    // Back to 0; a drag that carries on starts from there.
                    self.set(cx, 0);
                    if self.drag.is_some() {
                        self.drag = Some((cx.mouse().cursory, 0));
                    }
                    meta.consume();
                }
                WindowEvent::MouseMove(_, y) => {
                    if let Some((origin, start)) = self.drag {
                        let steps = ((origin - y) / (self.px_per_step * cx.scale_factor())).trunc() as i32;
                        let value = (start + steps).clamp(min, max);
                        if value != param.value() {
                            self.set(cx, value);
                        }
                    }
                }
                WindowEvent::MouseUp(MouseButton::Left) if self.drag.is_some() => {
                    self.drag = None;
                    cx.release();
                    cx.emit(ParamEvent::EndSetParameter(param).upcast());
                    meta.consume();
                }
                WindowEvent::MouseScroll(_, y) => {
                    self.scroll += y;
                    let steps = self.scroll.trunc();
                    if steps != 0.0 {
                        self.scroll -= steps;
                        let value = (param.value() + steps as i32).clamp(min, max);
                        if value != param.value() {
                            self.set(cx, value);
                        }
                    }
                    meta.consume();
                }
                _ => {}
            });
        });
    }
}

/// Tabler "star" outline (in vizia's bundled icon font, not in its list of names).
const ICON_STAR: &str = "\u{eb2e}";
/// Tabler "key" and "bug" (the font has them; vizia names only some).
const ICON_KEY: &str = "\u{eac7}";
const ICON_BUG: &str = "\u{ea48}";

/// Tabler "external-link" (in vizia's bundled icon font, not in its list of names).
const ICON_EXTERNAL_LINK: &str = "\u{ea99}";

/// Saves the current clip and reveals it in Finder.
fn show_in_finder(cx: &mut Context) {
    Button::new(cx, |cx| cx.emit(AppEvent::ShowInFinder), |cx| Icon::new(cx, ICON_EXTERNAL_LINK))
        .class("icon-button")
        .top(Pixels(5.0));
}

/// Stems and chops cards for songs; one wide waveform tile for short files.
fn song_cards(cx: &mut Context) {
    // -1: not a song (or not analysed yet); otherwise the analysis version.
    let song_version = AppData::shared.map(|s| {
        let key = s.selection.locked().key.clone();
        let has = s.song_view.locked().as_ref().is_some_and(|v| v.key == key);
        if has {
            s.song_view_version.load(Ordering::Relaxed) as i64
        } else {
            -1
        }
    });
    Binding::new(cx, AppData::selected_is_song, move |cx, is_song| {
        if !is_song.get(cx) {
            loop_card(cx);
            return;
        }
        Binding::new(cx, song_version, |cx, version| {
            if version.get(cx) < 0 {
                VStack::new(cx, |cx| {
                    Label::new(cx, "Finding chops…");
                })
                .class("card")
                .class("empty-state")
                .height(Auto);
                return;
            }
            let shared = AppData::shared.get(cx);
            let Some(view) = shared.song_view.locked().clone() else { return };
            // Development: TEMPO_SAMPLER_DEMO_CHOP=<n> selects tile n (1-based) of
            // the current length once.
            static DEMO_CHOP_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if let Some(n) = std::env::var("TEMPO_SAMPLER_DEMO_CHOP").ok().and_then(|n| n.parse::<usize>().ok()) {
                if !DEMO_CHOP_DONE.swap(true, Ordering::Relaxed) {
                    let length = chop_length_lens().get(cx);
                    let index = (0..view.chops.len()).filter(|&i| view.chops[i].bars == length).nth(n.saturating_sub(1));
                    if let Some(i) = index {
                        cx.emit(AppEvent::SelectChop(i));
                    }
                }
            }
            stems_card(cx);
            chops_card(cx, shared, view);
        });
    });
}

fn stems_card(cx: &mut Context) {
    VStack::new(cx, |cx| {
        centred(HStack::new(cx, |cx| {
            Icon::new(cx, ICON_ADJUSTMENTS_ALT).class("lead-icon");
            Label::new(cx, "Stems").width(Stretch(1.0));
            Label::new(
                cx,
                // Progress while splitting; nothing once ready.
                AppData::shared.map(|s| {
                    let key = s.selection.locked().key.clone();
                    let (k, st) = s.stem_state.locked().clone();
                    match st {
                        _ if k != key => String::new(),
                        StemState::Loading => "Loading…".into(),
                        StemState::Splitting => {
                            format!("Splitting {:.0}%", s.stem_progress.load(Ordering::Relaxed) * 100.0)
                        }
                        StemState::Failed(_) => "Split failed".into(),
                        StemState::Ready | StemState::Idle => String::new(),
                    }
                }),
            )
            .class("note");
        }))
        .class("card-title")
        .col_between(Pixels(8.0));

        HStack::new(cx, |cx| {
            // One lookup shared by the four buttons (lenses run every frame).
            let ready = AppData::shared.map(|s| {
                let sel = s.selection.locked();
                s.stems_for(&sel.key).is_some()
            });
            for (i, name) in STEM_NAMES.iter().enumerate() {
                let label = {
                    let mut c = name.chars();
                    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
                };
                let on = AppData::shared.map(move |s| s.stem_switches.load(Ordering::Relaxed) & (1 << i) != 0);
                // On/off button: indicator light and name. A plain row
                // (not a vizia Button) so the name can fill the middle.
                centred(HStack::new(cx, move |cx| {
                    Element::new(cx)
                        .class("led")
                        .class(name)
                        .top(Stretch(1.0))
                        .bottom(Stretch(1.0))
                        .hoverable(false);
                    Label::new(cx, &label).class("stem-name").width(Stretch(1.0)).hoverable(false);
                }))
                .class("stem")
                .col_between(Pixels(8.0))
                .child_left(Pixels(16.0))
                .child_right(Pixels(16.0))
                .width(Stretch(1.0))
                .height(Pixels(40.0))
                .checked(on)
                .toggle_class("disabled", ready.map(|r| !*r))
                .on_press(move |cx| cx.emit(AppEvent::ToggleStem(i)));
            }
        })
        .col_between(Pixels(8.0))
        .height(Auto);
    })
    .class("card")
    .height(Auto);
}

fn chops_card(cx: &mut Context, shared: Arc<Shared>, view: Arc<crate::SongView>) {
    VStack::new(cx, |cx| {
        centred(HStack::new(cx, |cx| {
            Icon::new(cx, ICON_CUT).class("lead-icon");
            Label::new(cx, "Chops").width(Stretch(1.0));
            HStack::new(cx, |cx| {
                for bars in [2usize, 4, 1, crate::one_shots::ONE_SHOT] {
                    let label = match bars {
                        crate::one_shots::ONE_SHOT => "One shots".to_string(),
                        1 => "1 bar".to_string(),
                        _ => format!("{bars} bars"),
                    };
                    Button::new(cx, move |cx| cx.emit(AppEvent::SetChopLength(bars)), move |cx| Label::new(cx, &label))
                        .checked(chop_length_lens().map(move |l| *l == bars));
                }
            })
            .class("segmented");
            // Hold to play the whole song; drag to export it.
            PressDrag::new(cx, AppEvent::WholeSong, |cx| {
                Label::new(cx, "Whole song").hoverable(false);
            })
            .class("whole-song")
            // Raised only while held (playing), like the tiles.
            .checked(AppData::root.map(|d| d.selected_chop < 0 && d.shared.preview.load(Ordering::Relaxed)));
        }))
        .class("card-title")
        .col_between(Pixels(8.0));

        // Tiles for the chosen length.
        Binding::new(cx, chop_length_lens(), move |cx, length| {
            let length = length.get(cx);
            let indices: Vec<usize> = (0..view.chops.len()).filter(|&i| view.chops[i].bars == length).collect();
            VStack::new(cx, |cx| {
                for row in indices.chunks(TILES_PER_ROW) {
                    HStack::new(cx, |cx| {
                        for &i in row {
                            ChopTile::new(cx, shared.clone(), i, &view);
                        }
                        // Keep tiles the same width on a short last row.
                        for _ in row.len()..TILES_PER_ROW {
                            Element::new(cx).width(Stretch(1.0));
                        }
                    })
                    .col_between(Pixels(8.0))
                    .height(Pixels(TILE_HEIGHT));
                }
            })
            .row_between(Pixels(8.0))
            .height(Auto);
            if indices.is_empty() && length == crate::one_shots::ONE_SHOT {
                Label::new(cx, "No one-shots in what's playing").class("tiles-empty");
            }
        });
    })
    .class("card")
    .height(Auto);
}

/// The last action's feedback (export, drag, errors); hidden when there's none.
fn status_line(cx: &mut Context) {
    Label::new(cx, AppData::message)
        .class("status-line")
        .text_wrap(false)
        .toggle_class("hidden", AppData::message.map(|m| m.is_empty()));
}

/// Short files: one wide tile with the whole (stretched) file, in the same
/// style as the chop tiles. Click to play, drag into Ableton.
fn loop_card(cx: &mut Context) {
    VStack::new(cx, |cx| {
        centred(HStack::new(cx, |cx| {
            Icon::new(cx, ICON_WAVE_SINE).class("lead-icon");
            Label::new(cx, AppData::selected_is_loop.map(|l| if *l { "Loop" } else { "Sample" }))
                .width(Stretch(1.0));
        }))
        .class("card-title")
        .col_between(Pixels(8.0));
        let shared = AppData::shared.get(cx);
        LoopTile::new(cx, shared).height(Pixels(LOOP_TILE_HEIGHT));
    })
    .class("card")
    .height(Auto);
}

const LOOP_TILE_HEIGHT: f32 = 152.0;

struct LoopTile {
    hold: Hold,
}

impl LoopTile {
    fn new(cx: &mut Context, shared: Arc<Shared>) -> Handle<'_, Self> {
        Self { hold: Hold::default() }
            .build(cx, move |cx| {
                wave_frame(cx, |cx| {
                    LoopWave { shared }
                        .build(cx, |_| {})
                        .hoverable(false)
                        .bind(
                            AppData::shared.map(|s| {
                                let playhead = (s.playhead.load(Ordering::Relaxed) * 4000.0) as i64;
                                let preview = s.preview.load(Ordering::Relaxed);
                                (playhead, preview, s.is_up_to_date())
                            }),
                            |mut h, _| h.needs_redraw(),
                        );
                });
            })
            .width(Stretch(1.0))
            .checked(AppData::shared.map(|s| s.preview.load(Ordering::Relaxed)))
    }
}

impl View for LoopTile {
    fn element(&self) -> Option<&'static str> {
        Some("chop-tile")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        self.hold.event(cx, event, AppEvent::Audition);
    }
}

/// Waveform of the current render (what plays and what drags), with the
/// playhead while previewing.
struct LoopWave {
    shared: Arc<Shared>,
}

impl View for LoopWave {
    fn element(&self) -> Option<&'static str> {
        Some("chop-wave")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &mut Canvas) {
        let b = cx.bounds();
        if b.w <= 1.0 || b.h <= 1.0 {
            return;
        }
        let key = self.shared.selection.locked().key.clone();
        let Some((rendered, overview)) = self.shared.current_with_overview() else { return };
        if rendered.key != key || overview.is_empty() {
            return; // still stretching this one
        }
        let playing = self.shared.preview.load(Ordering::Relaxed);
        let colour = if playing { vg::Color::rgb(0x1a, 0x1a, 0x1a) } else { vg::Color::rgb(0x8a, 0x8a, 0x8a) };
        draw_overview(canvas, b, &overview, colour);

        let p = self.shared.playhead.load(Ordering::Relaxed);
        if playing && (0.0..=1.0).contains(&p) {
            let mut line = vg::Path::new();
            line.rect(b.x + b.w * p - 1.0, b.y, 2.0, b.h);
            canvas.fill_path(&line, &vg::Paint::color(vg::Color::rgb(0x00, 0x5b, 0xd3)));
        }
    }
}

/// A recessed panel that frames a waveform inside a tile.
fn wave_frame(cx: &mut Context, content: impl FnOnce(&mut Context)) {
    VStack::new(cx, content).class("wave-frame").height(Stretch(1.0)).hoverable(false);
}

/// The waveform as one filled outline (top edge left to right, bottom edge
/// back), scaled to the overview's peak: one path per tile instead of a
/// rectangle per pixel column. At most one point per overview entry.
fn draw_overview(canvas: &mut Canvas, b: BoundingBox, overview: &[(f32, f32)], colour: vg::Color) {
    let peak = overview.iter().fold(0.0f32, |m, &(lo, hi)| m.max(-lo).max(hi)).max(1e-4);
    let mid = b.y + b.h / 2.0;
    let half = b.h / 2.0 - 1.0;
    let columns = (b.w.max(1.0) as usize).min(overview.len()).max(1);
    let step = b.w / columns as f32;
    let edges: Vec<(f32, f32, f32)> = (0..columns)
        .map(|col| {
            let i0 = col * overview.len() / columns;
            let i1 = ((col + 1) * overview.len() / columns).max(i0 + 1).min(overview.len());
            let (lo, hi) = overview[i0..i1].iter().fold((0.0f32, 0.0f32), |(lo, hi), &(a, z)| (lo.min(a), hi.max(z)));
            // At least a hairline where it's silent.
            let (top, bottom) = (mid - hi / peak * half, mid - lo / peak * half);
            let pad = ((1.0 - (bottom - top)) / 2.0).max(0.0);
            (b.x + (col as f32 + 0.5) * step, top - pad, bottom + pad)
        })
        .collect();
    let mut path = vg::Path::new();
    path.move_to(b.x, edges[0].1);
    for &(x, top, _) in &edges {
        path.line_to(x, top);
    }
    path.line_to(b.x + b.w, edges[columns - 1].1);
    path.line_to(b.x + b.w, edges[columns - 1].2);
    for &(x, _, bottom) in edges.iter().rev() {
        path.line_to(x, bottom);
    }
    path.line_to(b.x, edges[0].2);
    path.close();
    canvas.fill_path(&path, &vg::Paint::color(colour));
}

/// Whether chop tile `index` is playing: held with the mouse, or its MIDI pad
/// is down. Only then does it look selected.
fn tile_playing(d: &AppData, index: usize) -> bool {
    (d.selected_chop == index as i64 && d.shared.preview.load(Ordering::Relaxed))
        || (index < 64 && d.shared.pads_down.load(Ordering::Relaxed) >> index & 1 == 1)
}

/// 1-based position of chop `index` among chops of the same length.
fn chop_number(chops: &[crate::chops::Chop], index: usize) -> usize {
    let bars = chops[index].bars;
    chops[..index].iter().filter(|c| c.bars == bars).count() + 1
}

const TILES_PER_ROW: usize = 5;
const TILE_HEIGHT: f32 = 96.0;

/// One chop: its waveform and a label. Click to audition, press and drag
/// to drag its WAV into Ableton.
struct ChopTile {
    index: usize,
    hold: Hold,
}

impl ChopTile {
    fn new<'a>(
        cx: &'a mut Context,
        shared: Arc<Shared>,
        index: usize,
        view: &crate::SongView,
    ) -> Handle<'a, Self> {
        let chop = &view.chops[index];
        let label = if chop.bars == crate::one_shots::ONE_SHOT {
            // Where it is in the song, "1:23".
            let secs = chop.start / view.sample_rate.max(1) as usize;
            format!("{}:{:02}", secs / 60, secs % 60)
        } else {
            format!("Bar {}", chop.first_bar + 1)
        };
        Self { index, hold: Hold::default() }
            .build(cx, move |cx| {
                wave_frame(cx, |cx| {
                    ChopWave::new(cx, shared, index)
                        .hoverable(false)
                        // Redraw when this tile's playhead moves (only the
                        // selected tile shows one, so the rest don't follow
                        // the playhead), when it's (de)selected, or when the
                        // stem switches change its waveform. Atomics only.
                        .bind(
                            AppData::root.map(move |d| {
                                let selected = tile_playing(d, index);
                                let playhead = if selected {
                                    (d.shared.playhead.load(Ordering::Relaxed) * 4000.0) as i64
                                } else {
                                    -1
                                };
                                (selected, playhead, d.shared.stem_switches.load(Ordering::Relaxed))
                            }),
                            |mut h, _| h.needs_redraw(),
                        );
                });
                Label::new(cx, &label).text_wrap(false).height(Pixels(16.0)).hoverable(false);
            })
            .width(Stretch(1.0))
            // Highlighted only while it's held and playing; the choice is
            // still remembered (for the export button) without showing.
            .checked(AppData::root.map(move |d| tile_playing(d, index)))
    }
}

impl View for ChopTile {
    fn element(&self) -> Option<&'static str> {
        Some("chop-tile")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        self.hold.event(cx, event, AppEvent::SelectChop(self.index));
    }
}

/// Draws one chop's waveform, plus the playhead while it's the one playing.
struct ChopWave {
    shared: Arc<Shared>,
    index: usize,
}

impl ChopWave {
    fn new(cx: &mut Context, shared: Arc<Shared>, index: usize) -> Handle<'_, Self> {
        Self { shared, index }.build(cx, |_| {})
    }
}

impl View for ChopWave {
    fn element(&self) -> Option<&'static str> {
        Some("chop-wave")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &mut Canvas) {
        let b = cx.bounds();
        if b.w <= 1.0 || b.h <= 1.0 {
            return;
        }
        let Some(view) = self.shared.song_view.locked().clone() else { return };
        // With some stems off, show the waveform of what's actually playing.
        let (mask, selected) = {
            let sel = self.shared.selection.locked();
            (sel.stem_mask, sel.chop.as_ref().map(|c| c.index))
        };
        let mixed = if mask != ALL_STEMS { self.shared.mixed_chop_overviews(mask) } else { None };
        let overview = match &mixed {
            Some(m) => m.get(self.index),
            None => view.chop_overviews.get(self.index),
        };
        let (Some(chop), Some(overview)) = (view.chops.get(self.index), overview) else {
            return;
        };
        if overview.is_empty() {
            return;
        }

        // The chop being held (playing) in the accent colour, others grey.
        let held = (selected == Some(self.index) && self.shared.preview.load(Ordering::Relaxed))
            || (self.index < 64 && self.shared.pads_down.load(Ordering::Relaxed) >> self.index & 1 == 1);
        let colour = if held {
            vg::Color::rgb(0x1a, 0x1a, 0x1a)
        } else {
            vg::Color::rgb(0x8a, 0x8a, 0x8a)
        };
        draw_overview(canvas, b, overview, colour);

        // Playhead, if this chop is the one previewing.
        let p = self.shared.playhead.load(Ordering::Relaxed);
        if held && p >= 0.0 && view.frames > 0 {
            let (a, z) = (chop.start as f32 / view.frames as f32, chop.end as f32 / view.frames as f32);
            if z > a && (a..=z).contains(&p) {
                let x = b.x + b.w * (p - a) / (z - a);
                let mut line = vg::Path::new();
                line.rect(x - 1.0, b.y, 2.0, b.h);
                canvas.fill_path(&line, &vg::Paint::color(vg::Color::rgb(0x00, 0x5b, 0xd3)));
            }
        }
    }
}

/// Emits `on_press` when pressed, and `AppEvent::StartDrag` if the mouse
/// then moves a few pixels with the button held. The press is handled
/// first, so the drag exports what was just selected.
struct PressDrag {
    on_press: AppEvent,
    hold: Hold,
}

impl PressDrag {
    fn new(cx: &mut Context, on_press: AppEvent, content: impl FnOnce(&mut Context)) -> Handle<'_, Self> {
        Self { on_press, hold: Hold::default() }.build(cx, content)
    }
}

/// Press, hold and drag, shared by rows and tiles. Press emits `on_press`
/// (which selects and starts the preview), letting go stops the preview,
/// and moving a few pixels while held starts a drag into the host.
#[derive(Default)]
struct Hold {
    armed: bool,
    origin: (f32, f32),
}

impl Hold {
    fn event(&mut self, cx: &mut EventContext, event: &mut Event, on_press: AppEvent) {
        crate::safety::guard("ui event", || {
            event.map(|window_event, meta| match window_event {
                WindowEvent::MouseDown(MouseButton::Left) => {
                    cx.emit(on_press.clone());
                    self.armed = true;
                    self.origin = (cx.mouse().cursorx, cx.mouse().cursory);
                    cx.capture();
                    meta.consume();
                }
                WindowEvent::MouseMove(x, y) if self.armed => {
                    if (x - self.origin.0).hypot(y - self.origin.1) > 4.0 {
                        self.armed = false;
                        cx.release();
                        cx.emit(AppEvent::StartDrag);
                    }
                }
                WindowEvent::MouseUp(MouseButton::Left) if self.armed => {
                    self.armed = false;
                    cx.release();
                    cx.emit(AppEvent::StopPreview);
                }
                _ => {}
            });
        });
    }
}

impl View for PressDrag {
    fn element(&self) -> Option<&'static str> {
        Some("press-drag")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        crate::safety::guard("ui event", || {
            if let AppEvent::SelectSlot(slot) = self.on_press {
                event.map(|window_event, _| match window_event {
                    WindowEvent::MouseEnter => cx.emit(AppEvent::HoverSlot(slot)),
                    WindowEvent::MouseLeave => cx.emit(AppEvent::UnhoverSlot(slot)),
                    _ => {}
                });
            }
            self.hold.event(cx, event, self.on_press.clone());
        });
    }
}

/// Wraps the vizia editor to (1) remember the native parent view the host
/// gives us in `spawn()`, which the OS drag API needs, and (2) run cleanup
/// when the window closes.
struct ParentViewEditor {
    inner: Box<dyn Editor>,
    parent_view: ParentView,
    shared: Arc<Shared>,
}

impl Editor for ParentViewEditor {
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn Any + Send> {
        #[cfg(target_os = "macos")]
        if let ParentWindowHandle::AppKitNsView(view) = parent {
            self.parent_view.store(view, Ordering::Release);
        }
        let handle = self.inner.spawn(parent, context);
        Box::new(ClearOnClose {
            parent_view: self.parent_view.clone(),
            shared: self.shared.clone(),
            _inner: handle,
        })
    }

    fn size(&self) -> (u32, u32) {
        self.inner.size()
    }

    fn set_scale_factor(&self, factor: f32) -> bool {
        self.inner.set_scale_factor(factor)
    }

    fn param_value_changed(&self, id: &str, normalized_value: f32) {
        self.inner.param_value_changed(id, normalized_value)
    }

    fn param_modulation_changed(&self, id: &str, modulation_offset: f32) {
        self.inner.param_modulation_changed(id, modulation_offset)
    }

    fn param_values_changed(&self) {
        self.inner.param_values_changed()
    }
}

/// Editor handle dropped when the window closes. Stops the preview (closing
/// the window ends the audition) and forgets the parent view so we never
/// touch a view the host has destroyed.
struct ClearOnClose {
    parent_view: ParentView,
    shared: Arc<Shared>,
    _inner: Box<dyn Any + Send>,
}

impl Drop for ClearOnClose {
    fn drop(&mut self) {
        self.shared.preview.store(false, Ordering::Relaxed);
        self.parent_view.store(std::ptr::null_mut(), Ordering::Release);
    }
}
