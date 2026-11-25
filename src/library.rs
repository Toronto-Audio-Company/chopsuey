//! The loop library: folders to scan, the loops found in them, and where
//! each loop's tempo came from.
//!
//! Settings and the detection cache live in
//! `~/Library/Application Support/Tempo Sampler/library.json`, shared by every
//! plugin instance and every Ableton set.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::bpm::{self, NameBpm};

pub const BUILTIN_KEY: &str = "builtin:test-loop";
/// Longer files are listed but not analysed or stretched (DJ mixes, stems of
/// whole albums...). Stretching takes memory proportional to length.
pub const MAX_FILE_SECS: f32 = 20.0 * 60.0;
const MAX_FOLDER_DEPTH: usize = 10;

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub folders: Vec<PathBuf>,
    /// Old absolute tempo corrections, by file path. Read only to migrate
    /// them to `tempo_scale`; dropped when the sample's scale is next set.
    #[serde(default)]
    pub overrides: HashMap<String, f32>,
    /// The ½× / 1× / 2× choice per file path (1× isn't stored).
    #[serde(default)]
    pub tempo_scale: HashMap<String, f32>,
    /// Detection results, by file path. Reused while size and mtime match.
    #[serde(default)]
    pub detected: HashMap<String, Detected>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Detected {
    pub size: u64,
    pub modified: u64,
    pub bpm: Option<f32>,
    /// Detection agreed with a bare number in the name, and the name's value was used.
    #[serde(default)]
    pub from_name: bool,
    /// `bpm::DETECTOR_VERSION` that produced this; older results are redone.
    #[serde(default)]
    pub version: u32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BpmSource {
    Builtin,
    FileName,
    Detected,
    /// Waiting for detection.
    Pending,
    /// Detection found nothing usable.
    Unknown,
}

impl BpmSource {
    pub fn label(self) -> &'static str {
        match self {
            BpmSource::Builtin => "built-in",
            BpmSource::FileName => "name",
            BpmSource::Detected => "detected",
            BpmSource::Pending => "detecting…",
            BpmSource::Unknown => "unknown",
        }
    }
}

#[derive(Clone)]
pub struct Entry {
    /// File path as a string, or `BUILTIN_KEY`.
    pub key: String,
    pub path: Option<PathBuf>,
    pub name: String,
    /// Name of the containing folder, for display.
    pub folder: String,
    pub secs: f32,
    /// Effective tempo: `base_bpm × scale`.
    pub bpm: Option<f32>,
    /// Tempo from the name or detection.
    pub base_bpm: Option<f32>,
    /// The user's ½× / 1× / 2× choice.
    pub scale: f32,
    pub source: BpmSource,
}

/// The ½× / 1× / 2× choices.
pub const TEMPO_SCALES: [f32; 3] = [0.5, 1.0, 2.0];

/// The saved scale for `key`, or one derived from an old absolute override.
fn scale_for(config: &Config, key: &str, base: Option<f32>) -> f32 {
    if let Some(&s) = config.tempo_scale.get(key) {
        return s;
    }
    match (config.overrides.get(key), base) {
        (Some(&o), Some(b)) if b > 0.0 => {
            let ratio = o / b;
            TEMPO_SCALES
                .into_iter()
                .min_by(|x, y| (ratio.ln() - x.ln()).abs().total_cmp(&(ratio.ln() - y.ln()).abs()))
                .unwrap_or(1.0)
        }
        _ => 1.0,
    }
}

impl Entry {
    /// Length in beats: whole beats for loops, exact for everything else.
    pub fn beats(&self) -> Option<f32> {
        self.bpm.map(|b| {
            let beats = self.secs * b / 60.0;
            if self.is_loop() { beats.round().max(1.0) } else { beats }
        })
    }

    /// A seamless loop of whole bars (vs. a song, chop or other clip).
    pub fn is_loop(&self) -> bool {
        self.bpm.is_some_and(|b| bpm::is_loop(self.secs, b))
    }

    /// Can be selected, previewed and stretched.
    pub fn usable(&self) -> bool {
        self.bpm.is_some() && self.secs <= MAX_FILE_SECS
    }

    fn builtin() -> Self {
        Self {
            key: BUILTIN_KEY.into(),
            path: None,
            name: "Test Loop".into(),
            folder: "built-in".into(),
            secs: 4.0,
            bpm: Some(120.0),
            base_bpm: Some(120.0),
            scale: 1.0,
            source: BpmSource::Builtin,
        }
    }
}

pub struct LibraryState {
    pub config: Config,
    pub entries: Vec<Entry>,
    pub scanning: bool,
    loaded: bool,
}

pub struct Library {
    pub state: Mutex<LibraryState>,
    /// Bumped whenever `entries` change, so the GUI knows to refresh.
    pub version: AtomicU64,
    /// Bumped per scan; a running scan stops when it sees a newer one.
    scan_generation: AtomicU64,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            state: Mutex::new(LibraryState {
                config: Config::default(),
                entries: vec![Entry::builtin()],
                scanning: false,
                loaded: false,
            }),
            version: AtomicU64::new(1),
            scan_generation: AtomicU64::new(0),
        }
    }
}

impl Library {
    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Loads settings from disk and scans, the first time only.
    pub fn ensure_loaded(self: &Arc<Self>) {
        {
            let mut st = self.state.lock().unwrap();
            if st.loaded {
                return;
            }
            st.loaded = true;
            st.config = load_config();
        }
        self.rescan();
    }

    /// Rescans all folders on a new thread, cancelling any scan in progress.
    pub fn rescan(self: &Arc<Self>) {
        let generation = self.scan_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let lib = self.clone();
        std::thread::Builder::new()
            .name("tempo-sampler-scan".into())
            .spawn(move || lib.scan(generation))
            .ok();
    }

    pub fn add_folders(self: &Arc<Self>, folders: Vec<PathBuf>) {
        if folders.is_empty() {
            return;
        }
        {
            let mut st = self.state.lock().unwrap();
            for f in folders {
                if !st.config.folders.contains(&f) {
                    st.config.folders.push(f);
                }
            }
            save_config(&st.config);
        }
        self.rescan();
    }

    pub fn remove_folder(self: &Arc<Self>, index: usize) {
        {
            let mut st = self.state.lock().unwrap();
            if index < st.config.folders.len() {
                st.config.folders.remove(index);
            }
            save_config(&st.config);
        }
        self.rescan();
    }

    pub fn entry(&self, key: &str) -> Option<Entry> {
        self.state.lock().unwrap().entries.iter().find(|e| e.key == key).cloned()
    }

    /// Sets a sample's ½× / 1× / 2× tempo choice. Saved to disk.
    pub fn set_scale(&self, key: &str, scale: f32) {
        if key == BUILTIN_KEY {
            return;
        }
        let mut st = self.state.lock().unwrap();
        st.config.overrides.remove(key);
        if scale == 1.0 {
            st.config.tempo_scale.remove(key);
        } else {
            st.config.tempo_scale.insert(key.to_string(), scale);
        }
        if let Some(e) = st.entries.iter_mut().find(|e| e.key == key) {
            e.scale = scale;
            e.bpm = e.base_bpm.map(|b| b * scale);
        }
        save_config(&st.config);
        drop(st);
        self.bump();
    }

    fn cancelled(&self, generation: u64) -> bool {
        self.scan_generation.load(Ordering::Relaxed) != generation
    }

    fn scan(&self, generation: u64) {
        let config = {
            let mut st = self.state.lock().unwrap();
            st.scanning = true;
            st.config.clone()
        };
        self.bump();

        // Pass 1: list files and read headers only. Fast.
        let mut files = Vec::new();
        for folder in &config.folders {
            walk(folder, 0, &mut files);
        }
        let mut entries = vec![Entry::builtin()];
        for path in files {
            if self.cancelled(generation) {
                return;
            }
            // Header only (fast), any supported format.
            let Ok((secs, _)) = crate::audio_file::probe(&path) else { continue };
            let key = path.to_string_lossy().into_owned();
            let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let folder = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();

            let (base_bpm, source) = {
                match bpm::bpm_from_name(&name) {
                    Some(NameBpm::Explicit(b)) => (Some(b), BpmSource::FileName),
                    Some(NameBpm::Bare(b)) if bpm::is_whole_bars(secs, b) => {
                        (Some(b), BpmSource::FileName)
                    }
                    _ => match config.detected.get(&key) {
                        Some(d)
                            if d.version == bpm::DETECTOR_VERSION
                                && Some((d.size, d.modified)) == file_stamp(&path) =>
                        {
                            match d.bpm {
                                Some(b) if d.from_name => (Some(b), BpmSource::FileName),
                                Some(b) => (Some(b), BpmSource::Detected),
                                None => (None, BpmSource::Unknown),
                            }
                        }
                        _ => (None, BpmSource::Pending),
                    },
                }
            };
            let scale = scale_for(&config, &key, base_bpm);
            let bpm = base_bpm.map(|b| b * scale);
            entries.push(Entry { key, path: Some(path), name, folder, secs, bpm, base_bpm, scale, source });
        }
        entries[1..].sort_by_key(|e| e.name.to_lowercase());
        // (key, path, length, bare tempo from the name if any)
        let pending: Vec<(String, PathBuf, f32, Option<f32>)> = entries
            .iter()
            .filter(|e| e.source == BpmSource::Pending)
            .filter_map(|e| {
                let hint = match bpm::bpm_from_name(&e.name) {
                    Some(NameBpm::Bare(b)) => Some(b),
                    _ => None,
                };
                Some((e.key.clone(), e.path.clone()?, e.secs, hint))
            })
            .collect();
        {
            let mut st = self.state.lock().unwrap();
            // Checked under the lock so a stale scan can't overwrite a newer one.
            if self.cancelled(generation) {
                return;
            }
            st.entries = entries;
        }
        self.bump();

        // Pass 2: analyse files that have no tempo in their name.
        let mut last_bump = Instant::now();
        for (key, path, secs, hint) in pending {
            if self.cancelled(generation) {
                return;
            }
            let detected = if secs <= MAX_FILE_SECS { detect_file(&path) } else { None };
            // A bare number in the name ("Soul Sample 88") is trusted when
            // detection lands on it or its half/double, which also settles
            // which octave is right.
            let from_name = match (detected, hint) {
                (Some(d), Some(h)) => [h, h * 2.0, h / 2.0].iter().any(|&c| bpm::close(d, c, 0.03)),
                _ => false,
            };
            let bpm = if from_name { hint } else { detected };
            let mut st = self.state.lock().unwrap();
            let scale = scale_for(&st.config, &key, bpm);
            if let Some(e) = st.entries.iter_mut().find(|e| e.key == key) {
                e.base_bpm = bpm;
                e.scale = scale;
                e.bpm = bpm.map(|b| b * scale);
                e.source = match (bpm, from_name) {
                    (Some(_), true) => BpmSource::FileName,
                    (Some(_), false) => BpmSource::Detected,
                    (None, _) => BpmSource::Unknown,
                };
            }
            if let Some((size, modified)) = file_stamp(&path) {
                let version = bpm::DETECTOR_VERSION;
                st.config.detected.insert(key, Detected { size, modified, bpm, from_name, version });
            }
            drop(st);
            if last_bump.elapsed() > Duration::from_millis(250) {
                self.bump();
                last_bump = Instant::now();
            }
        }

        let mut st = self.state.lock().unwrap();
        if self.cancelled(generation) {
            return;
        }
        st.scanning = false;
        save_config(&st.config);
        drop(st);
        self.bump();
    }
}

fn detect_file(path: &Path) -> Option<f32> {
    let (stereo, sr) = crate::audio_file::decode(path).ok()?;
    let mono: Vec<f32> = stereo.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
    bpm::detect_bpm(&mono, sr)
}

fn file_stamp(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some((meta.len(), modified))
}

/// Collects .wav files under `dir`. Skips hidden entries and doesn't follow
/// symlinks (avoids loops in the folder tree).
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_FOLDER_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            walk(&path, depth + 1, out);
        } else if ft.is_file()
            && crate::audio_file::is_supported(&path)
        {
            out.push(path);
        }
    }
}

fn config_path() -> PathBuf {
    // Lets tests use a scratch file instead of the real library settings.
    if let Some(p) = std::env::var_os("TEMPO_SAMPLER_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    home.join("Library/Application Support/Tempo Sampler/library.json")
}

fn load_config() -> Config {
    std::fs::read(config_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Writes atomically (temp file + rename) so a crash can't leave half a file.
fn save_config(config: &Config) {
    let path = config_path();
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_vec_pretty(config) else { return };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::LoopAsset;

    fn write_wav(path: &Path, samples: &[f32], sample_rate: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        samples.iter().for_each(|s| w.write_sample(*s).unwrap());
        w.finalize().unwrap();
    }

    #[test]
    fn scans_folder_and_finds_tempos() {
        let dir = std::env::temp_dir().join(format!("tempo-sampler-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("TEMPO_SAMPLER_CONFIG", dir.join("library.json"));

        let base = LoopAsset::test_loop();
        let samples = dir.join("samples");
        let at = |bpm| base.stretch_to(bpm).samples;
        write_wav(&samples.join("Drums 124bpm.wav"), &at(124.0), 48_000);
        write_wav(&samples.join("sub/loop_140_Cmin.wav"), &at(140.0), 48_000);
        write_wav(&samples.join("mystery.wav"), &at(95.0), 48_000);
        std::fs::write(samples.join("notes.txt"), "not audio").unwrap();

        let lib = Arc::new(Library::default());
        lib.ensure_loaded();
        lib.add_folders(vec![samples.clone()]);

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            {
                let st = lib.state.lock().unwrap();
                let done = st.entries.len() == 4
                    && !st.scanning
                    && st.entries.iter().all(|e| e.source != BpmSource::Pending);
                if done {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "scan did not finish");
            std::thread::sleep(Duration::from_millis(50));
        }

        let get = |name: &str| lib.state.lock().unwrap().entries.iter().find(|e| e.name == name).cloned().unwrap();
        let e = get("Drums 124bpm");
        assert_eq!((e.bpm, e.source), (Some(124.0), BpmSource::FileName));
        let e = get("loop_140_Cmin");
        assert_eq!((e.bpm, e.source), (Some(140.0), BpmSource::FileName));
        assert_eq!(e.folder, "sub");
        let e = get("mystery");
        assert_eq!((e.bpm, e.source), (Some(95.0), BpmSource::Detected));
        assert_eq!(e.beats(), Some(8.0));

        // Settings and the detection cache were saved.
        let saved = load_config();
        assert_eq!(saved.folders, vec![samples.clone()]);
        assert!(saved.detected.values().any(|d| d.bpm == Some(95.0)));

        // A ½× choice sticks and is saved; 1× clears it.
        let key = e.key.clone();
        lib.set_scale(&key, 0.5);
        let e = lib.entry(&key).unwrap();
        assert_eq!((e.bpm, e.base_bpm, e.scale), (Some(47.5), Some(95.0), 0.5));
        assert_eq!(load_config().tempo_scale.get(&key), Some(&0.5));
        lib.set_scale(&key, 1.0);
        assert_eq!(lib.entry(&key).unwrap().bpm, Some(95.0));
        assert!(load_config().tempo_scale.get(&key).is_none());

        // An old absolute override becomes the nearest scale.
        let mut config = Config::default();
        config.overrides.insert("k".into(), 190.0);
        assert_eq!(scale_for(&config, "k", Some(95.0)), 2.0);
        assert_eq!(scale_for(&config, "k", None), 1.0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
