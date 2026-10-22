//! One window per user session. The first launch holds a named lock; a later launch signals a
//! named event, which asks the first to show its window, and exits.

use std::io;
use std::thread;

use cxtune::Error;
use cxtune::lock::NamedLock;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
use windows::core::HSTRING;

const NAME: &str = r"Local\cxtune-gui";

/// Held by the first launch for as long as it runs.
pub struct Guard {
    _lock: NamedLock,
    /// Never closed: the thread that waits on it lives as long as the process.
    event: HANDLE,
}

pub enum Outcome {
    First(Guard),
    /// Another launch is running and has been asked to show its window.
    Second,
}

pub fn acquire() -> io::Result<Outcome> {
    acquire_named(NAME)
}

fn acquire_named(name: &str) -> io::Result<Outcome> {
    // Created before the lock, so that a later launch that finds the lock held finds the event.
    // SAFETY: a plain Win32 call; the handle is kept or closed below.
    let event = unsafe { CreateEventW(None, false, false, &HSTRING::from(format!("{name}-show"))) }
        .map_err(io::Error::other)?;
    match NamedLock::acquire(name) {
        Ok(lock) => Ok(Outcome::First(Guard { _lock: lock, event })),
        Err(Error::Busy(_)) => {
            // This launch was started by the user, so it may hand the right to come to the
            // front over to the running one.
            // SAFETY: plain Win32 calls on a handle that was just created.
            unsafe {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                let signalled = SetEvent(event);
                let _ = windows::Win32::Foundation::CloseHandle(event);
                signalled.map_err(io::Error::other)?;
            }
            Ok(Outcome::Second)
        }
        Err(other) => Err(io::Error::other(other)),
    }
}

impl Guard {
    /// Calls `on_show` on another thread each time a later launch asks for the window.
    pub fn listen(&self, on_show: impl Fn() + Send + 'static) {
        let event = self.event.0 as usize;
        thread::spawn(move || {
            // SAFETY: the handle stays open for the life of the process.
            while unsafe { WaitForSingleObject(HANDLE(event as *mut _), INFINITE) } == WAIT_OBJECT_0
            {
                on_show();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_later_launch_asks_the_first_to_show_its_window_and_the_name_is_free_again_afterwards() {
        let name = format!(r"Local\cxtune-gui-test-{}", std::process::id());
        let Outcome::First(first) = acquire_named(&name).unwrap() else {
            panic!("nothing runs under this name yet");
        };
        let (shown, requests) = mpsc::channel();
        first.listen(move || {
            let _ = shown.send(());
        });
        assert!(requests.recv_timeout(Duration::from_millis(200)).is_err());

        for _ in 0..2 {
            assert!(matches!(acquire_named(&name).unwrap(), Outcome::Second));
            requests
                .recv_timeout(Duration::from_secs(5))
                .expect("the first launch is asked to show its window");
        }

        drop(first);
        assert!(matches!(acquire_named(&name).unwrap(), Outcome::First(_)));
    }
}
