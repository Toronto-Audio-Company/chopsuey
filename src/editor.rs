use nih_plug::prelude::{Editor, GuiContext, ParentWindowHandle};
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::widgets::*;
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};
use std::any::Any;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

use crate::{drag, HostTransport, Shared, TempoSamplerParams};

/// The native view (NSView* on macOS) the host opened our editor in.
/// Null while the editor is closed. Needed to start OS drags.
type ParentView = Arc<AtomicPtr<c_void>>;

/// GUI-side model. vizia re-evaluates every bound lens (including `.map()`
/// closures) once per frame and redraws when the result changes, so binding
/// labels to closures that read the atomics is enough to keep them live.
/// (Timers don't work here: vizia's plugin-window backend never ticks them.)
#[derive(Lens)]
struct Data {
    shared: Arc<Shared>,
    params: Arc<TempoSamplerParams>,
    /// Result of the last export, shown under the button.
    export_message: String,
    #[lens(ignore)]
    parent_view: ParentView,
}

enum AppEvent {
    TogglePreview,
    ShowInFinder,
    StartDrag,
}

impl Model for Data {
    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|e, _| match e {
            AppEvent::TogglePreview => {
                self.shared.preview.fetch_xor(true, Ordering::Relaxed);
            }
            AppEvent::StartDrag => {
                let result = self.shared.export_current().and_then(|path| {
                    drag::start_file_drag(self.parent_view.load(Ordering::Acquire), &path)
                });
                if let Err(e) = result {
                    self.export_message = format!("Drag failed: {e}");
                }
            }
            AppEvent::ShowInFinder => {
                self.export_message = match self.shared.export_current() {
                    Ok(path) => {
                        let _ = std::process::Command::new("open").arg("-R").arg(&path).spawn();
                        format!("Saved {}", path.file_name().unwrap_or_default().to_string_lossy())
                    }
                    Err(e) => format!("Export failed: {e}"),
                };
            }
        });
    }
}

pub(crate) fn default_state() -> Arc<ViziaState> {
    ViziaState::new(|| (400, 440))
}

fn time_sig(t: &HostTransport) -> (i32, i32) {
    (
        t.time_sig_numerator.load(Ordering::Relaxed),
        t.time_sig_denominator.load(Ordering::Relaxed),
    )
}

/// "bar.beat" display, e.g. "5.3", using the host time signature.
fn format_position(pos_beats: f64, (num, den): (i32, i32)) -> String {
    // pos_beats is in quarter notes; one bar is num * (4 / den) quarter notes.
    let quarters_per_beat = 4.0 / den.max(1) as f64;
    let beats = pos_beats / quarters_per_beat;
    let num = num.max(1) as f64;
    let bar = (beats / num).floor() as i64 + 1;
    let beat = (beats.rem_euclid(num)).floor() as i64 + 1;
    format!("{bar}.{beat}")
}

fn render_status(s: &Shared) -> String {
    let rendered = s.rendered_bpm.load(Ordering::Relaxed);
    let host = s.transport.tempo.load(Ordering::Relaxed);
    if rendered <= 0.0 {
        "Stretching…".to_string()
    } else if rendered != host {
        format!("Re-stretching for {host:.2} BPM…")
    } else {
        let secs = s.asset.beats * 60.0 / rendered;
        format!("Stretched to {rendered:.2} BPM · {secs:.2} s")
    }
}

pub(crate) fn create(
    shared: Arc<Shared>,
    params: Arc<TempoSamplerParams>,
    editor_state: Arc<ViziaState>,
) -> Option<Box<dyn Editor>> {
    let parent_view: ParentView = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
    let gui_parent_view = parent_view.clone();
    let close_shared = shared.clone();

    let inner = create_vizia_editor(editor_state, ViziaTheming::Custom, move |cx, _| {
        assets::register_noto_sans_light(cx);
        assets::register_noto_sans_thin(cx);

        Data {
            shared: shared.clone(),
            params: params.clone(),
            export_message: String::new(),
            parent_view: gui_parent_view.clone(),
        }
        .build(cx);

        let asset = &shared.asset;
        let loop_info = format!(
            "{} · {} bars @ {} BPM",
            asset.name,
            asset.beats / 4.0,
            asset.bpm
        );

        VStack::new(cx, |cx| {
            Label::new(cx, "Tempo Sampler")
                .font_family(vec![FamilyOwned::Name(String::from(assets::NOTO_SANS))])
                .font_weight(FontWeightKeyword::Thin)
                .font_size(24.0)
                .height(Pixels(36.0));

            Label::new(
                cx,
                Data::shared.map(|s| {
                    let t = s.transport.tempo.load(Ordering::Relaxed);
                    if t > 0.0 {
                        format!("{t:.2} BPM")
                    } else {
                        "-- BPM".to_string()
                    }
                }),
            )
            .font_size(44.0)
            .height(Pixels(60.0));

            HStack::new(cx, |cx| {
                Label::new(
                    cx,
                    Data::shared.map(|s| {
                        if s.transport.playing.load(Ordering::Relaxed) {
                            "Playing"
                        } else {
                            "Stopped"
                        }
                    }),
                );
                Label::new(
                    cx,
                    Data::shared.map(|s| {
                        let (n, d) = time_sig(&s.transport);
                        format!("{n}/{d}")
                    }),
                );
                Label::new(
                    cx,
                    Data::shared.map(|s| {
                        let t = &s.transport;
                        format_position(t.pos_beats.load(Ordering::Relaxed), time_sig(t))
                    }),
                );
            })
            .col_between(Pixels(16.0))
            .height(Auto)
            .child_left(Stretch(1.0))
            .child_right(Stretch(1.0));

            // Loop section
            VStack::new(cx, |cx| {
                Label::new(cx, &loop_info);
                Label::new(cx, Data::shared.map(|s| render_status(s)));

                HStack::new(cx, |cx| {
                    Button::new(
                        cx,
                        |cx| cx.emit(AppEvent::TogglePreview),
                        |cx| {
                            Label::new(
                                cx,
                                Data::shared.map(|s| {
                                    if s.preview.load(Ordering::Relaxed) {
                                        "Stop preview"
                                    } else {
                                        "Play preview"
                                    }
                                }),
                            )
                        },
                    )
                    .width(Pixels(120.0))
                    .background_color(Data::shared.map(|s| {
                        if s.preview.load(Ordering::Relaxed) {
                            Color::rgb(255, 214, 165) // playing: warm highlight
                        } else {
                            Color::rgb(235, 235, 235)
                        }
                    }));
                    ParamSlider::new(cx, Data::params, |p| &p.gain).width(Pixels(160.0));
                })
                .col_between(Pixels(10.0))
                .height(Auto)
                .child_left(Stretch(1.0))
                .child_right(Stretch(1.0));

                DragHandle::new(cx, |cx| {
                    Label::new(cx, "Drag WAV into Ableton").hoverable(false);
                })
                .width(Pixels(220.0))
                .height(Pixels(40.0))
                .child_space(Stretch(1.0))
                .background_color(Color::rgb(222, 232, 246))
                .border_color(Color::rgb(120, 150, 200))
                .border_width(Pixels(1.0))
                .border_radius(Pixels(6.0));

                Button::new(
                    cx,
                    |cx| cx.emit(AppEvent::ShowInFinder),
                    |cx| Label::new(cx, "Show WAV in Finder"),
                );
                Label::new(cx, Data::export_message).font_size(11.0);
            })
            .row_between(Pixels(8.0))
            .height(Auto)
            .top(Pixels(14.0))
            .child_left(Stretch(1.0))
            .child_right(Stretch(1.0));
        })
        .row_between(Pixels(6.0))
        .child_left(Stretch(1.0))
        .child_right(Stretch(1.0))
        .child_top(Pixels(16.0));

        ResizeHandle::new(cx);
    })?;

    Some(Box::new(ParentViewEditor {
        inner,
        parent_view,
        shared: close_shared,
    }))
}

/// A press-and-drag area that emits `AppEvent::StartDrag` once the mouse has
/// moved a few pixels with the button held.
struct DragHandle {
    armed: bool,
    origin: Option<(f32, f32)>,
}

impl DragHandle {
    fn new(cx: &mut Context, content: impl FnOnce(&mut Context)) -> Handle<'_, Self> {
        Self { armed: false, origin: None }.build(cx, content)
    }
}

impl View for DragHandle {
    fn element(&self) -> Option<&'static str> {
        Some("drag-handle")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                self.armed = true;
                self.origin = None;
                cx.capture();
                meta.consume();
            }
            WindowEvent::MouseMove(x, y) if self.armed => {
                let (ox, oy) = *self.origin.get_or_insert((*x, *y));
                if (x - ox).hypot(y - oy) > 4.0 {
                    self.armed = false;
                    cx.release();
                    cx.emit(AppEvent::StartDrag);
                }
            }
            WindowEvent::MouseUp(MouseButton::Left) if self.armed => {
                self.armed = false;
                cx.release();
            }
            _ => {}
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
