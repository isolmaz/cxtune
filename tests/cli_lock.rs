//! The command line must stay away from the dongle while another process controls it.

use std::path::PathBuf;
use std::process::Command;

use cxtune::Error;
use cxtune::lock::NamedLock;

fn cxtune() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cxtune"))
}

/// A directory that is removed when the test ends, also when it fails.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// One test, so that no other test of this file takes the lock for a moment in between.
#[test]
fn every_device_command_is_refused_while_the_lock_is_held() {
    // Either this test holds the lock or a running cxtune does; both make the lock busy.
    let _held = NamedLock::device();
    assert!(matches!(NamedLock::device(), Err(Error::Busy(_))));

    let appdata =
        Scratch(std::env::temp_dir().join(format!("cxtune-cli-lock-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&appdata.0);
    std::fs::create_dir_all(&appdata.0).unwrap();

    let commands: [&[&str]; 9] = [
        &["status"],
        &["apply", "balanced"],
        &["tune", "300"],
        &["mic-boost", "6"],
        &["mic-nr", "off"],
        &["mic-agc", "on"],
        &["sidetone", "on"],
        &["sidetone-level", "100"],
        &["watch"],
    ];
    for args in commands {
        let out = cxtune()
            .args(args)
            .env("APPDATA", &appdata.0)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(
            stderr.contains("busy") && stderr.contains("controlling the dongle"),
            "{args:?}: {stderr}"
        );
    }
    // a refused command writes no config
    assert_eq!(std::fs::read_dir(&appdata.0).unwrap().count(), 0);

    // the usage message needs no device and so no lock
    let out = cxtune().output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: cxtune"));
}
