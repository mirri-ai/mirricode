//! Thread-local panic location capture for `catch_unwind` surfaces.
//!
//! A caught panic payload (`Box<dyn Any + Send>`) carries only the message —
//! the file/line lives in the `PanicInfo` the default hook prints to stderr,
//! which the desktop-embedded host discards. This module installs a chained
//! panic hook that records the location into the panicking thread's
//! thread-local, so `catch_unwind` handlers (the subagent host, the tool
//! scheduler) can append " (panic at file:line)" to the surfaced error.

use std::cell::RefCell;

thread_local! {
    static LAST_PANIC_LOCATION: RefCell<Option<String>> = const { RefCell::new(None) };
}

static INSTALL: std::sync::Once = std::sync::Once::new();

/// Install the process-wide panic hook (idempotent): record the panic
/// location into the panicking thread's thread-local, log via tracing,
/// then chain to the previous hook so default stderr output is unchanged.
pub fn install_panic_location_capture() {
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|location| location.to_string())
                .unwrap_or_else(|| "unknown location".to_string());
            let message = payload_string(info.payload());
            LAST_PANIC_LOCATION.with(|slot| {
                *slot.borrow_mut() = Some(location.clone());
            });
            tracing::error!(panic_location = %location, %message, "panic captured");
            previous(info);
        }));
    });
}

/// Take (and clear) the current thread's last captured panic location —
/// the `catch_unwind` handler runs on the panicking thread, so the
/// thread-local read after a catch matches the captured panic.
pub fn take_last_panic_location() -> Option<String> {
    LAST_PANIC_LOCATION.with(|slot| slot.borrow_mut().take())
}

fn payload_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_an_installed_hook_when_a_panic_is_caught_then_the_location_is_taken() {
        install_panic_location_capture();
        let result = std::panic::catch_unwind(|| {
            panic!("boom");
        });
        assert!(result.is_err());
        let location = take_last_panic_location().expect("location captured");
        assert!(location.contains("panic_capture.rs"), "got {location}");
        // Reading consumed it
        assert!(take_last_panic_location().is_none());
    }
}
