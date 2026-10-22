//! Start with Windows: a value in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` that
//! starts this program minimized in the notification area.

use std::io;
use std::path::Path;

use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "cxtune";

/// The command line Windows runs at sign-in: the program in quotes, because its path may hold
/// spaces, and `--minimized`.
fn command(exe: &Path) -> String {
    format!("\"{}\" --minimized", exe.display())
}

pub fn is_enabled() -> io::Result<bool> {
    is_enabled_at(RUN_KEY, &std::env::current_exe()?)
}

pub fn set_enabled(enabled: bool) -> io::Result<()> {
    set_enabled_at(RUN_KEY, &std::env::current_exe()?, enabled)
}

/// Whether the value under `key` starts `exe`. A value that starts another program, such as
/// this one from another folder, does not count.
fn is_enabled_at(key: &str, exe: &Path) -> io::Result<bool> {
    let run = match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(key, KEY_READ) {
        Ok(run) => run,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match run.get_value::<String, _>(VALUE) {
        Ok(value) => Ok(value == command(exe)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn set_enabled_at(key: &str, exe: &Path, enabled: bool) -> io::Result<()> {
    let (run, _) =
        RegKey::predef(HKEY_CURRENT_USER).create_subkey_with_flags(key, KEY_SET_VALUE)?;
    if enabled {
        run.set_value(VALUE, &command(exe))
    } else {
        match run.delete_value(VALUE) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::UI::Shell::CommandLineToArgvW;
    use windows::core::HSTRING;

    use super::*;

    /// How Windows splits a command line into arguments.
    fn arguments(line: &str) -> Vec<String> {
        let mut count = 0;
        // SAFETY: the array is read within `count` and freed once.
        unsafe {
            let argv = CommandLineToArgvW(&HSTRING::from(line), &mut count);
            assert!(!argv.is_null());
            let arguments = (0..count as usize)
                .map(|i| (*argv.add(i)).to_string().unwrap())
                .collect();
            LocalFree(Some(HLOCAL(argv.cast())));
            arguments
        }
    }

    #[test]
    fn the_program_is_one_argument_whatever_its_path_holds() {
        for path in [
            r"C:\Program Files\cxtune\cxtune-gui.exe",
            r"C:\Users\İsmail Çağlar\Masaüstü\cxtune gui.exe",
            r"D:\a\b.exe",
        ] {
            assert_eq!(
                arguments(&command(Path::new(path))),
                [path, "--minimized"],
                "{path}"
            );
        }
    }

    /// A key of the test's own, removed when the test ends, also when it fails.
    struct TestKey(String);

    impl TestKey {
        fn new(tag: &str) -> TestKey {
            TestKey(format!(
                r"Software\cxtune-gui-test\{tag}-{}",
                std::process::id()
            ))
        }
    }

    impl Drop for TestKey {
        fn drop(&mut self) {
            let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(&self.0);
        }
    }

    #[test]
    fn registering_and_unregistering_round_trips_through_the_registry() {
        let key = TestKey::new("round-trip");
        let exe = PathBuf::from(r"C:\Program Files\cxtune\cxtune-gui.exe");
        assert!(!is_enabled_at(&key.0, &exe).unwrap());

        set_enabled_at(&key.0, &exe, true).unwrap();
        assert!(is_enabled_at(&key.0, &exe).unwrap());
        let (run, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey_with_flags(&key.0, KEY_READ)
            .unwrap();
        assert_eq!(
            run.get_value::<String, _>(VALUE).unwrap(),
            r#""C:\Program Files\cxtune\cxtune-gui.exe" --minimized"#
        );

        set_enabled_at(&key.0, &exe, false).unwrap();
        assert!(!is_enabled_at(&key.0, &exe).unwrap());
        assert!(run.get_value::<String, _>(VALUE).is_err());
        // unregistering what is not registered is not an error
        set_enabled_at(&key.0, &exe, false).unwrap();
    }

    #[test]
    fn a_value_that_starts_another_copy_of_the_program_does_not_count() {
        let key = TestKey::new("other-copy");
        let here = PathBuf::from(r"C:\Tools\cxtune-gui.exe");
        let elsewhere = PathBuf::from(r"C:\Old\cxtune-gui.exe");
        set_enabled_at(&key.0, &elsewhere, true).unwrap();
        assert!(!is_enabled_at(&key.0, &here).unwrap());

        // registering again points the value at this copy
        set_enabled_at(&key.0, &here, true).unwrap();
        assert!(is_enabled_at(&key.0, &here).unwrap());
        assert!(!is_enabled_at(&key.0, &elsewhere).unwrap());
    }

    #[test]
    fn a_missing_key_means_not_registered() {
        let key = TestKey::new("missing");
        assert!(!is_enabled_at(&key.0, Path::new(r"C:\x.exe")).unwrap());
    }
}
