//! Keeping a bug in chopsuey from taking the host down with it.
//!
//! A Rust panic that unwinds into the host's C++ (a VST3 or CLAP call, a GUI
//! event from the host's run loop) aborts the whole process: Live quits and
//! the user loses their unsaved set. So every way into our code catches
//! panics at its edge (`guard`, `spawn`, the `process` wrapper in lib.rs),
//! logs them, and carries on with that one piece of work abandoned.
//!
//! A thread that panics while holding a `Mutex` poisons it, and with
//! `lock().unwrap()` every later lock panics too. `locked()` takes the lock
//! anyway: the data is whatever the panicking code left, which for our
//! caches and selections is always usable (at worst stale).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::logln;

pub trait LockExt<T: ?Sized> {
    /// `lock()` that recovers from poisoning instead of panicking.
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> LockExt<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Runs `f`, catching a panic. `None` if it panicked (already logged with
/// its backtrace by the panic hook; this adds which job it was).
pub fn guard<R>(what: &str, f: impl FnOnce() -> R) -> Option<R> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(_) => {
            logln!("recovered from a panic in {what}");
            None
        }
    }
}

/// Starts a named background thread whose panics are caught and logged.
pub fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    let label = name.to_string();
    let result = std::thread::Builder::new().name(name.to_string()).spawn(move || {
        guard(&label, f);
    });
    if let Err(e) = result {
        logln!("couldn't start thread {name}: {e}");
    }
}

/// Logs every panic with its thread and a backtrace, then defers to the
/// previous hook. Installed once per process when the plugin is created.
pub fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let backtrace = std::backtrace::Backtrace::force_capture();
            logln!("PANIC: {info}\n{backtrace}");
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_lock_still_opens() {
        let m = std::sync::Arc::new(Mutex::new(5));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.locked();
            panic!("poison it");
        })
        .join();
        assert!(m.is_poisoned());
        assert_eq!(*m.locked(), 5);
    }

    #[test]
    fn guard_catches() {
        assert_eq!(guard("test", || 3), Some(3));
        assert_eq!(guard("test", || -> i32 { panic!("boom") }), None);
    }
}
