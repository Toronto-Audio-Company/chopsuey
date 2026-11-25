use atomic_float::{AtomicF32, AtomicF64};
use nih_plug::prelude::*;
use nih_plug_vizia::ViziaState;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

mod drag;
mod editor;
pub mod loops;

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

/// State shared by the audio thread, the background render thread and the GUI.
pub struct Shared {
    pub transport: HostTransport,
    pub asset: LoopAsset,
    /// Most recent tempo the audio thread asked to render. Lets the render
    /// thread skip requests that were superseded while queued.
    latest_request: AtomicF32,
    /// Tempo of the most recently finished render, 0.0 before the first one.
    pub rendered_bpm: AtomicF32,
    /// Whether the loop preview is playing. GUI-only state rather than a host
    /// parameter: it isn't saved with the set, doesn't create undo steps, and
    /// is switched off when the window closes or Ableton's transport stops.
    pub preview: AtomicBool,
    /// Latest render, for the GUI (export / drag). Never touched by the audio thread.
    current: Mutex<Option<Arc<RenderedLoop>>>,
    /// New renders travel to the audio thread through this lock-free queue...
    to_audio: Mutex<rtrb::Producer<Arc<RenderedLoop>>>,
    /// ...and the ones it replaces come back here, so their memory is freed
    /// on the render thread instead of the audio thread.
    garbage: Mutex<rtrb::Consumer<Arc<RenderedLoop>>>,
}

impl Shared {
    /// Runs on nih-plug's background thread.
    fn render(&self, bpm: f32) {
        if self.latest_request.load(Ordering::Relaxed) != bpm {
            return; // a newer tempo was requested while this one was queued
        }
        while self.garbage.lock().unwrap().pop().is_ok() {}

        let rendered = Arc::new(self.asset.stretch_to(bpm));
        *self.current.lock().unwrap() = Some(rendered.clone());
        // If the queue is full the audio thread isn't running; the GUI still
        // has the render, and the next one will get through.
        let _ = self.to_audio.lock().unwrap().push(rendered);
        self.rendered_bpm.store(bpm, Ordering::Relaxed);
    }

    /// Writes the latest render to the export folder and returns its path.
    pub fn export_current(&self) -> Result<PathBuf, String> {
        let rendered = self.current.lock().unwrap().clone();
        let rendered = rendered.ok_or("Nothing rendered yet")?;
        rendered.write_wav(&loops::export_dir()).map_err(|e| e.to_string())
    }
}

pub enum Task {
    Render(f32),
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
    /// Smoothed preview on/off level, 0.0..=1.0.
    preview_level: f32,
    preview_fade_coef: f32,
}

#[derive(Params)]
pub struct TempoSamplerParams {
    /// Window size, saved with the Ableton set.
    #[persist = "editor-state"]
    editor_state: Arc<ViziaState>,

    #[id = "gain"]
    pub gain: FloatParam,
}

impl Default for TempoSampler {
    fn default() -> Self {
        let (to_audio, from_render) = rtrb::RingBuffer::new(8);
        let (to_garbage, garbage) = rtrb::RingBuffer::new(16);
        let shared = Shared {
            transport: HostTransport::default(),
            asset: LoopAsset::test_loop(),
            latest_request: AtomicF32::new(0.0),
            rendered_bpm: AtomicF32::new(0.0),
            preview: AtomicBool::new(false),
            current: Mutex::new(None),
            to_audio: Mutex::new(to_audio),
            garbage: Mutex::new(garbage),
        };
        Self {
            params: Arc::new(TempoSamplerParams {
                editor_state: editor::default_state(),
                gain: FloatParam::new(
                    "Preview Gain",
                    util::db_to_gain(-6.0),
                    FloatRange::Skewed {
                        min: util::db_to_gain(-36.0),
                        max: util::db_to_gain(6.0),
                        factor: FloatRange::gain_skew_factor(-36.0, 6.0),
                    },
                )
                .with_smoother(SmoothingStyle::Logarithmic(50.0))
                .with_unit(" dB")
                .with_value_to_string(formatters::v2s_f32_gain_to_db(1))
                .with_string_to_value(formatters::s2v_f32_gain_to_db()),
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
            Task::Render(bpm) => shared.render(bpm),
        })
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(
            self.shared.clone(),
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
                    shared.latest_request.store(tempo, Ordering::Relaxed);
                    context.execute_background(Task::Render(tempo));
                }
            }
        }

        // 3. Pick up finished renders (keep only the newest).
        while let Ok(new_loop) = self.from_render.pop() {
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
        let num_samples = buffer.samples();

        for (i, mut frame) in buffer.iter_samples().enumerate() {
            let gain = self.params.gain.smoothed.next();
            let target = if preview_on { 1.0 } else { 0.0 };
            self.preview_level += (target - self.preview_level) * self.preview_fade_coef;

            let Some(lp) = self.playing_loop.as_deref() else { continue };
            if self.preview_level < 1e-5 {
                continue;
            }
            let frames = lp.frames();
            // Where are we inside the loop? Map beats -> fraction of loop -> frame.
            // Because this is beat-based, it stays in sync even mid-tempo-change
            // and handles host/loop sample-rate differences for free.
            let beats = start_beats + i as f64 * beats_per_sample;
            let phase = (beats / lp.beats as f64).rem_euclid(1.0);
            let pos = phase * frames as f64;
            let i0 = (pos as usize).min(frames - 1);
            let i1 = (i0 + 1) % frames;
            let frac = (pos - i0 as f64) as f32;
            let level = gain * self.preview_level;
            for ch in 0..2 {
                let s = lp.samples[i0 * 2 + ch] * (1.0 - frac) + lp.samples[i1 * 2 + ch] * frac;
                if let Some(o) = frame.get_mut(ch) {
                    *o += s * level;
                }
            }
        }
        self.free_beats = start_beats + num_samples as f64 * beats_per_sample;

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
