//! Settings in `%APPDATA%\cxtune\config.toml`: the selected mode with the times of its routine
//! (a custom setup is a mode with times other than the routine's defaults), mic boost, the
//! microphone DSP settings and the virtual surround. The file uses `key = value` lines (a TOML
//! subset).
//! It is replaced as a whole by renaming a finished temporary file over it, so a reader or a
//! crash never sees a half-written file.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::mode::{self, Mode};
use crate::patches;
use crate::{Error, Result};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub mode: Option<Mode>,
    /// Mic boost step 0..=7; `None` leaves $1017 untouched.
    pub mic_boost: Option<u8>,
    /// Lowest-noise soft-start time in ms (key `soft_start_ms`); `None` keeps the routine's
    /// default. Applies while the mode is lowest noise.
    pub min_hiss_ms: Option<u16>,
    /// Balanced stop decay time in ms (key `decay_ms`); `None` keeps the routine's default.
    /// Applies while the mode is balanced.
    pub balanced_decay_ms: Option<u16>,
    /// Microphone noise reduction; `None` leaves it as the device has it.
    pub mic_nr: Option<bool>,
    /// Microphone AGC; `None` leaves it as the device has it.
    pub mic_agc: Option<bool>,
    /// Sidetone on or off; `None` leaves it as the device has it.
    pub sidetone: Option<bool>,
    /// Sidetone gain register value; `None` leaves the device's.
    pub sidetone_level: Option<u16>,
    /// Virtual surround on or off; `None` leaves it as the device has it.
    pub surround: Option<bool>,
    /// Virtual surround level 1-10; `None` leaves the device's.
    pub surround_level: Option<u8>,
}

pub fn path() -> Result<PathBuf> {
    let appdata =
        std::env::var_os("APPDATA").ok_or_else(|| Error::Config("APPDATA is not set".into()))?;
    Ok(PathBuf::from(appdata).join("cxtune").join("config.toml"))
}

pub fn load() -> Result<Config> {
    load_from(&path()?)
}

/// Reads the file at `path`; a missing file is the default settings. Every error names the file.
pub fn load_from(path: &Path) -> Result<Config> {
    let named = |why: &dyn std::fmt::Display| Error::Config(format!("{}: {why}", path.display()));
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|e| named(&e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(named(&e)),
    }
}

pub fn save(cfg: &Config) -> Result<()> {
    save_to(&path()?, cfg)
}

/// Writes `cfg` to a temporary file next to `path`, flushes it to disk and renames it over
/// `path`. On failure `path` is untouched and the temporary file is removed. Every error names
/// the file.
pub fn save_to(path: &Path, cfg: &Config) -> Result<()> {
    let io = |e: std::io::Error| Error::Config(format!("{}: {e}", path.display()));
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(io)?;
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(render(cfg).as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e)
    })
}

fn parse(text: &str) -> Result<Config> {
    let mut cfg = Config::default();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || Error::Config(format!("invalid line: {line}"));
        let (key, value) = line.split_once('=').ok_or_else(bad)?;
        let value = value.trim().trim_matches('"');
        match key.trim() {
            "mode" => cfg.mode = Some(Mode::parse(value).ok_or_else(bad)?),
            "mic_boost" => {
                let n: u8 = value.parse().map_err(|_| bad())?;
                if n > 7 {
                    return Err(bad());
                }
                cfg.mic_boost = Some(n);
            }
            "soft_start_ms" | "min_hiss_ms" => {
                let ms: u16 = value.parse().map_err(|_| bad())?;
                mode::settle_units(ms).map_err(|_| bad())?;
                cfg.min_hiss_ms = Some(ms);
            }
            "decay_ms" => {
                let ms: u16 = value.parse().map_err(|_| bad())?;
                mode::decay_units(ms).map_err(|_| bad())?;
                cfg.balanced_decay_ms = Some(ms);
            }
            "mic_nr" => cfg.mic_nr = Some(parse_bool(value).ok_or_else(bad)?),
            "mic_agc" => cfg.mic_agc = Some(parse_bool(value).ok_or_else(bad)?),
            "sidetone" => cfg.sidetone = Some(parse_bool(value).ok_or_else(bad)?),
            "sidetone_level" => {
                let level: u16 = value.parse().map_err(|_| bad())?;
                patches::sidetone_level(level).map_err(|_| bad())?;
                cfg.sidetone_level = Some(level);
            }
            "surround" => cfg.surround = Some(parse_bool(value).ok_or_else(bad)?),
            "surround_level" => {
                let level: u8 = value.parse().map_err(|_| bad())?;
                patches::surround_level(level).map_err(|_| bad())?;
                cfg.surround_level = Some(level);
            }
            _ => return Err(bad()),
        }
    }
    Ok(cfg)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn render(cfg: &Config) -> String {
    let mut s = String::new();
    if let Some(m) = cfg.mode {
        s += &format!("mode = \"{}\"\n", m.name());
    }
    if let Some(n) = cfg.mic_boost {
        s += &format!("mic_boost = {n}\n");
    }
    if let Some(ms) = cfg.min_hiss_ms {
        s += &format!("soft_start_ms = {ms}\n");
    }
    if let Some(ms) = cfg.balanced_decay_ms {
        s += &format!("decay_ms = {ms}\n");
    }
    for (key, value) in [
        ("mic_nr", cfg.mic_nr),
        ("mic_agc", cfg.mic_agc),
        ("sidetone", cfg.sidetone),
        ("surround", cfg.surround),
    ] {
        if let Some(on) = value {
            s += &format!("{key} = {on}\n");
        }
    }
    if let Some(level) = cfg.sidetone_level {
        s += &format!("sidetone_level = {level}\n");
    }
    if let Some(level) = cfg.surround_level {
        s += &format!("surround_level = {level}\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TempDir;

    #[test]
    fn round_trips_every_mode_and_option() {
        for mode in Mode::ALL {
            let cfg = Config {
                mode: Some(mode),
                mic_boost: Some(6),
                min_hiss_ms: Some(300),
                balanced_decay_ms: Some(500),
                mic_nr: Some(false),
                mic_agc: Some(true),
                sidetone: Some(true),
                sidetone_level: Some(947),
                surround: Some(true),
                surround_level: Some(7),
            };
            assert_eq!(parse(&render(&cfg)).unwrap(), cfg);
        }
        assert_eq!(
            parse(&render(&Config::default())).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn modes_are_written_by_their_names_and_read_by_those_and_the_older_ones() {
        for (mode, name) in [
            (Mode::Stock, "default"),
            (Mode::Quiet, "low-click"),
            (Mode::Balanced, "balanced"),
            (Mode::MinHiss, "lowest-noise"),
        ] {
            let cfg = Config {
                mode: Some(mode),
                ..Config::default()
            };
            assert_eq!(render(&cfg), format!("mode = \"{name}\"\n"));
            assert_eq!(parse(&render(&cfg)).unwrap(), cfg);
        }
        for (old, mode) in [
            ("stock", Mode::Stock),
            ("quiet", Mode::Quiet),
            ("click-free", Mode::Quiet),
            ("min-hiss", Mode::MinHiss),
        ] {
            assert_eq!(
                parse(&format!("mode = \"{old}\"\n")).unwrap().mode,
                Some(mode),
                "{old}"
            );
        }
    }

    #[test]
    fn rejects_out_of_range_and_unknown_values() {
        assert!(parse("mic_boost = 8").is_err());
        assert!(parse("mode = \"loud\"").is_err());
        assert!(parse("mode = \"clamp\"").is_err());
        assert!(parse("no_such_key = 1").is_err());
        for key in ["soft_start_ms", "min_hiss_ms"] {
            assert!(parse(&format!("{key} = 10")).is_err(), "{key}");
            assert!(parse(&format!("{key} = 2010")).is_err(), "{key}");
            assert!(parse(&format!("{key} = 605")).is_err(), "{key}");
            assert!(parse(&format!("{key} = 20")).is_ok(), "{key}");
            assert!(parse(&format!("{key} = 2000")).is_ok(), "{key}");
        }
        assert_eq!(
            parse("min_hiss_ms = 300").unwrap(),
            parse("soft_start_ms = 300").unwrap()
        );
        assert_eq!(
            render(&parse("min_hiss_ms = 300").unwrap()),
            "soft_start_ms = 300\n"
        );
    }

    #[test]
    fn the_decay_time_is_limited_to_the_proven_range_in_steps_of_ten() {
        for bad in ["0", "90", "95", "105", "2010", "2100", "-100", "long", ""] {
            assert!(parse(&format!("decay_ms = {bad}")).is_err(), "{bad}");
        }
        for ok in [100, 110, 800, 1990, 2000] {
            assert_eq!(
                parse(&format!("decay_ms = {ok}"))
                    .unwrap()
                    .balanced_decay_ms,
                Some(ok)
            );
        }
        assert_eq!(
            render(&Config {
                balanced_decay_ms: Some(800),
                ..Config::default()
            }),
            "decay_ms = 800\n"
        );
    }

    #[test]
    fn the_surround_accepts_only_booleans_and_levels_of_one_to_ten() {
        assert_eq!(parse("surround = true").unwrap().surround, Some(true));
        assert_eq!(parse("surround = false").unwrap().surround, Some(false));
        for bad in ["on", "1", "\"maybe\"", "True", ""] {
            assert!(parse(&format!("surround = {bad}")).is_err(), "{bad}");
        }
        for ok in 1..=10u8 {
            assert_eq!(
                parse(&format!("surround_level = {ok}"))
                    .unwrap()
                    .surround_level,
                Some(ok)
            );
        }
        for bad in ["0", "11", "255", "256", "-1", "loud", ""] {
            assert!(parse(&format!("surround_level = {bad}")).is_err(), "{bad}");
        }
    }

    #[test]
    fn dsp_settings_accept_only_booleans_and_levels_in_range() {
        for key in ["mic_nr", "mic_agc", "sidetone"] {
            assert!(parse(&format!("{key} = true")).is_ok());
            assert!(parse(&format!("{key} = false")).is_ok());
            for bad in ["on", "1", "\"maybe\"", "True", ""] {
                assert!(parse(&format!("{key} = {bad}")).is_err(), "{key} = {bad}");
            }
        }
        assert_eq!(parse("sidetone_level = 1").unwrap().sidetone_level, Some(1));
        assert_eq!(
            parse("sidetone_level = 4096").unwrap().sidetone_level,
            Some(4096)
        );
        for bad in ["0", "4097", "32768", "-1", "loud", ""] {
            assert!(parse(&format!("sidetone_level = {bad}")).is_err(), "{bad}");
        }
    }

    #[test]
    fn dsp_settings_that_are_not_set_are_not_written() {
        assert_eq!(render(&Config::default()), "");
        let cfg = Config {
            mic_nr: Some(false),
            ..Config::default()
        };
        assert_eq!(render(&cfg), "mic_nr = false\n");
        let cfg = Config {
            surround_level: Some(6),
            ..Config::default()
        };
        assert_eq!(render(&cfg), "surround_level = 6\n");
    }

    fn sample() -> Config {
        Config {
            mode: Some(Mode::MinHiss),
            mic_boost: Some(6),
            min_hiss_ms: Some(300),
            balanced_decay_ms: Some(400),
            mic_nr: Some(true),
            mic_agc: Some(false),
            sidetone: Some(false),
            sidetone_level: Some(500),
            surround: Some(false),
            surround_level: Some(5),
        }
    }

    #[test]
    fn a_saved_file_replaces_the_old_one_whole_and_leaves_no_temporary_file() {
        let dir = TempDir::new("config-replace");
        let file = dir.join("config.toml");
        let old = Config {
            mode: Some(Mode::Quiet),
            ..Config::default()
        };
        save_to(&file, &old).unwrap();
        assert_eq!(load_from(&file).unwrap(), old);

        // a handle on the old file keeps the complete old content: the new content arrives as
        // a different file, not as a rewrite of this one
        let mut reader = std::fs::File::open(&file).unwrap();
        save_to(&file, &sample()).unwrap();
        let mut seen = String::new();
        std::io::Read::read_to_string(&mut reader, &mut seen).unwrap();
        assert_eq!(seen, render(&old));
        assert_eq!(load_from(&file).unwrap(), sample());
        drop(reader);

        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.toml"]);
    }

    #[test]
    fn saving_creates_the_directory() {
        let dir = TempDir::new("config-create");
        let file = dir.join("nested").join("config.toml");
        save_to(&file, &sample()).unwrap();
        assert_eq!(load_from(&file).unwrap(), sample());
    }

    #[test]
    fn a_failed_save_names_the_file_and_leaves_the_old_one_and_no_temporary_file() {
        let dir = TempDir::new("config-failed");
        let file = dir.join("config.toml");
        let old = Config {
            mode: Some(Mode::Stock),
            ..Config::default()
        };
        save_to(&file, &old).unwrap();
        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&file, permissions).unwrap();

        let err = save_to(&file, &sample()).unwrap_err().to_string();
        assert!(err.contains(&file.display().to_string()), "{err}");
        assert_eq!(load_from(&file).unwrap(), old);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn an_unreadable_file_is_named_in_the_error() {
        let dir = TempDir::new("config-unreadable");
        let file = dir.join("config.toml");
        std::fs::write(&file, "profile = \"idle\"\n").unwrap();
        let err = load_from(&file).unwrap_err().to_string();
        assert!(err.contains(&file.display().to_string()), "{err}");
        assert!(err.contains("profile = \"idle\""), "{err}");
        assert_eq!(
            load_from(&dir.join("missing.toml")).unwrap(),
            Config::default()
        );
    }
}
