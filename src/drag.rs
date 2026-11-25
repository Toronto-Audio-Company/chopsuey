//! Dragging a file out of the plugin window into the host (or Finder).
//!
//! vizia/baseview can only receive file drops, not start them, so this talks
//! to the OS directly. macOS only for now; Windows would need an OLE
//! `DoDragDrop` equivalent.

use std::ffi::c_void;
use std::path::Path;

/// Starts an OS drag of the file at `path`. `parent_view` is the native view
/// the host gave us when it opened the editor. Must be called on the GUI
/// (main) thread while the mouse button is held.
pub fn start_file_drag(parent_view: *mut c_void, path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        mac::start_file_drag(parent_view, path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (parent_view, path);
        Err("Dragging out is only implemented on macOS so far".into())
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::OnceCell;
    use std::ffi::c_void;
    use std::path::Path;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2::{define_class, msg_send, AllocAnyThread, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession, NSDraggingSource,
        NSEvent, NSEventModifierFlags, NSEventType, NSView, NSWorkspace,
    };
    use objc2_foundation::{
        NSArray, NSObject, NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSSize, NSString,
        NSURL,
    };

    define_class!(
        // The drag source tells AppKit which operations we allow (copy).
        // The name must be unique in the host process.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "TempoSamplerDragSource_v1"]
        struct DragSource;

        unsafe impl NSObjectProtocol for DragSource {}

        unsafe impl NSDraggingSource for DragSource {
            #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
            fn source_operation_mask(
                &self,
                _session: &NSDraggingSession,
                _context: NSDraggingContext,
            ) -> NSDragOperation {
                NSDragOperation::Copy
            }
        }
    );

    impl DragSource {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }

    thread_local! {
        // One long-lived source object; AppKit only holds a weak-ish reference to it.
        static SOURCE: OnceCell<Retained<DragSource>> = const { OnceCell::new() };
    }

    pub fn start_file_drag(parent_view: *mut c_void, path: &Path) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or("drag must start on the main thread")?;
        if parent_view.is_null() {
            return Err("plugin window is not open".into());
        }
        // SAFETY: the host passed this NSView* to Editor::spawn, and we clear our
        // copy when the editor closes, so it is alive while the GUI runs.
        let view: &NSView = unsafe { &*(parent_view as *const NSView) };
        let window = view.window().ok_or("plugin view has no window")?;

        // vizia processes mouse input on its next frame rather than inside
        // AppKit's mouse handler, so there is no live mouse event to hand to
        // AppKit. Synthesize a left-drag event at the current cursor position.
        let loc_in_window = window.mouseLocationOutsideOfEventStream();
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            NSEventType::LeftMouseDragged,
            loc_in_window,
            NSEventModifierFlags::empty(),
            NSProcessInfo::processInfo().systemUptime(),
            window.windowNumber(),
            None,
            0,
            1,
            1.0,
        )
        .ok_or("could not create drag event")?;

        let path_str = path.to_str().ok_or("export path is not valid UTF-8")?;
        let ns_path = NSString::from_str(path_str);
        let url = NSURL::fileURLWithPath(&ns_path);

        // NSURL writes a file URL to the drag pasteboard, same as dragging from Finder.
        let item = NSDraggingItem::initWithPasteboardWriter(
            NSDraggingItem::alloc(),
            ProtocolObject::from_ref(&*url),
        );

        // Show the file's Finder icon under the cursor while dragging.
        let icon = NSWorkspace::sharedWorkspace().iconForFile(&ns_path);
        let p = view.convertPoint_fromView(loc_in_window, None);
        let size = 48.0;
        let frame = NSRect::new(
            NSPoint::new(p.x - size / 2.0, p.y - size / 2.0),
            NSSize::new(size, size),
        );
        let icon_obj: &AnyObject = icon.as_ref();
        unsafe { item.setDraggingFrame_contents(frame, Some(icon_obj)) };

        let items = NSArray::from_retained_slice(&[item]);
        SOURCE.with(|cell| {
            let source = cell.get_or_init(|| DragSource::new(mtm));
            let session = view.beginDraggingSessionWithItems_event_source(
                &items,
                &event,
                ProtocolObject::from_ref(&**source),
            );
            session.setAnimatesToStartingPositionsOnCancelOrFail(true);
        });
        Ok(())
    }
}
