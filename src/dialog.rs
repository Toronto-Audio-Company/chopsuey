//! Native "choose folder" dialog.

use std::path::PathBuf;

/// Opens a folder picker and returns immediately; `on_chosen` runs later
/// with the chosen folders (not at all if cancelled). GUI (main) thread only.
///
/// This must not block. The GUI runs inside baseview's window handler, which
/// is held in a `RefCell` while vizia processes our click. A blocking
/// `runModal()` would sit inside that borrow, and as soon as the panel takes
/// focus, AppKit sends the plugin window a focus event, baseview borrows
/// again, and the panic aborts the whole host. `beginWithCompletionHandler`
/// shows the panel without blocking, and its callback runs later from the
/// normal run loop, outside any borrow.
pub fn pick_folders(on_chosen: impl Fn(Vec<PathBuf>) + 'static) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use block2::RcBlock;
        use objc2::MainThreadMarker;
        use objc2_app_kit::{NSModalResponse, NSModalResponseOK, NSOpenPanel};
        use objc2_foundation::NSString;

        let mtm = MainThreadMarker::new().ok_or("dialog must open on the main thread")?;
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(true);
        panel.setPrompt(Some(&NSString::from_str("Add to Library")));
        panel.setMessage(Some(&NSString::from_str(
            "Choose folders of loops. Subfolders are included.",
        )));

        let panel_in_block = panel.clone();
        let handler = RcBlock::new(move |result: NSModalResponse| {
            if result != NSModalResponseOK {
                return;
            }
            let folders: Vec<PathBuf> = panel_in_block
                .URLs()
                .iter()
                .filter_map(|url| url.path())
                .map(|p| PathBuf::from(p.to_string()))
                .collect();
            on_chosen(folders);
        });
        panel.beginWithCompletionHandler(&handler);
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = on_chosen;
        Err("Folder picker is only implemented on macOS so far".into())
    }
}
