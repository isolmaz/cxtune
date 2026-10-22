//! Cross-process exclusivity through a named kernel mutex.
//!
//! The HID memory protocol has no request tags, so two processes talking to the dongle at once
//! can receive each other's responses. Whoever controls the dongle therefore holds
//! [`NamedLock::device`] for as long as it uses the device: a short-lived CLI command for the
//! length of the command, the engine for its whole life.
//!
//! The lock is the existence of the named mutex object, not ownership of it: whoever creates
//! the object first holds the lock, and every later `CreateMutexW` of the same name sees that
//! the object already exists. Windows destroys the object when the last handle closes, so a
//! process that exits or crashes releases the lock, and the handle can be dropped on any thread
//! (an owned mutex would belong to the thread that waited on it and be abandoned when that
//! thread ends). A process that is only probing closes its handle again at once.
//!
//! [`NamedLock::acquire`] takes any name, so the same primitive can guard other things: a
//! single GUI instance per session is `Local\...`, the device is machine-wide (`Global\...`).

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, HANDLE,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::HSTRING;

use crate::{Error, Result};

/// Name of the lock that guards the dongle. `Global\` makes it hold across Windows sessions,
/// since the dongle belongs to the whole machine.
const DEVICE: &str = "Global\\cxtune-device";

/// A held named lock. Dropping it releases the name.
#[derive(Debug)]
pub struct NamedLock(HANDLE);

// SAFETY: the handle is only closed on drop, which is valid on any thread.
unsafe impl Send for NamedLock {}

impl NamedLock {
    /// Takes the lock that guards the dongle, or fails with [`Error::Busy`] and a message that
    /// tells the user what to close.
    pub fn device() -> Result<NamedLock> {
        Self::acquire(DEVICE).map_err(|e| match e {
            Error::Busy(_) => Error::Busy(
                "another cxtune process is controlling the dongle (the tray app, `cxtune watch` \
                 or a command that is still running); close it and try again"
                    .into(),
            ),
            other => other,
        })
    }

    /// Takes the lock called `name`, or fails with [`Error::Busy`] if another handle to it
    /// exists, in this or any other process.
    pub fn acquire(name: &str) -> Result<NamedLock> {
        let wide = HSTRING::from(name);
        // SAFETY: plain Win32 calls; the handle is closed exactly once, here or in `Drop`.
        unsafe {
            match CreateMutexW(None, false, &wide) {
                Ok(handle) if GetLastError() == ERROR_ALREADY_EXISTS => {
                    let _ = CloseHandle(handle);
                    Err(busy(name))
                }
                Ok(handle) => Ok(NamedLock(handle)),
                // the object exists and belongs to a process of another user
                Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => Err(busy(name)),
                Err(e) => Err(Error::Device(format!(
                    "could not create the lock '{name}': {e}"
                ))),
            }
        }
    }
}

fn busy(name: &str) -> Error {
    Error::Busy(format!("the lock '{name}' is held by another process"))
}

impl Drop for NamedLock {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by `CreateMutexW` and is closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    /// A name that no other test or run uses.
    fn unique(scope: &str, tag: &str) -> String {
        format!("{scope}\\cxtune-test-{tag}-{}", std::process::id())
    }

    fn is_busy(r: &Result<NamedLock>) -> bool {
        matches!(r, Err(Error::Busy(_)))
    }

    #[test]
    fn a_held_name_refuses_a_second_taker_until_it_is_released() {
        let name = unique("Local", "held");
        let first = NamedLock::acquire(&name).unwrap();
        assert!(is_busy(&NamedLock::acquire(&name)));
        // a refused attempt does not release or disturb the holder
        assert!(is_busy(&NamedLock::acquire(&name)));
        drop(first);
        let again = NamedLock::acquire(&name).unwrap();
        assert!(is_busy(&NamedLock::acquire(&name)));
        drop(again);
    }

    #[test]
    fn different_names_do_not_conflict() {
        let a = NamedLock::acquire(&unique("Local", "a")).unwrap();
        let b = NamedLock::acquire(&unique("Local", "b")).unwrap();
        drop((a, b));
    }

    #[test]
    fn a_machine_wide_name_can_be_taken_without_elevation() {
        let name = unique("Global", "machine");
        let first = NamedLock::acquire(&name).unwrap();
        assert!(is_busy(&NamedLock::acquire(&name)));
        drop(first);
        drop(NamedLock::acquire(&name).unwrap());
    }

    #[test]
    fn the_lock_outlives_the_thread_that_took_it_and_can_be_released_on_another() {
        let name = unique("Local", "threads");
        let lock = thread::spawn({
            let name = name.clone();
            move || NamedLock::acquire(&name).unwrap()
        })
        .join()
        .unwrap();
        // the taker has ended; an owned mutex would be abandoned and free by now
        assert!(is_busy(&NamedLock::acquire(&name)));
        thread::spawn(move || drop(lock)).join().unwrap();
        drop(NamedLock::acquire(&name).unwrap());
    }

    #[test]
    fn a_lock_of_another_process_refuses_until_that_process_is_killed() {
        let name = unique("Local", "process");
        let script = format!(
            "$m = New-Object System.Threading.Mutex($false, '{name}'); \
             [Console]::Out.WriteLine('held'); [Console]::Out.Flush(); Start-Sleep -Seconds 120"
        );
        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(lines.next().unwrap().unwrap(), "held");

        assert!(is_busy(&NamedLock::acquire(&name)));

        child.kill().unwrap();
        child.wait().unwrap();
        // the kernel closes the dead process's handles; allow it a moment
        let deadline = Instant::now() + Duration::from_secs(5);
        let lock = loop {
            match NamedLock::acquire(&name) {
                Ok(lock) => break lock,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("the lock stayed held after the process died: {e}"),
            }
        };
        drop(lock);
    }
}
