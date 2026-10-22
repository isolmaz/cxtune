//! Settings in `%APPDATA%\cxtune\config.toml`: the selected mode, mic boost and min-hiss
//! settle time. The file uses `key = value` lines (a TOML subset).

use std::path::PathBuf;

use crate::mode::{self, Mode};
use crate::{Error, Result};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub mode: Option<Mode>,
    /// Mic boost step 0..=7; `None` leaves $1017 untouched.
    pub mic_boost: Option<u8>,
    /// Min-hiss settle time in ms; `None` keeps the routine's default.
    pub min_hiss_ms: Option<u16>,
}

pub fn path() -> Result<PathBuf> {
    let appdata =
        std::env::var_os("APPDATA").ok_or_else(|| Error::Config("APPDATA is not set".into()))?;
    Ok(PathBuf::from(appdata).join("cxtune").join("config.toml"))
}

pub fn load() -> Result<Config> {
    let path = path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(Error::Config(format!("{}: {e}", path.display()))),
    }
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = path()?;
    let io = |e: std::io::Error| Error::Config(format!("{}: {e}", path.display()));
    std::fs::create_dir_all(path.parent().unwrap()).map_err(io)?;
    std::fs::write(&path, render(cfg)).map_err(io)
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
            "min_hiss_ms" => {
                let ms: u16 = value.parse().map_err(|_| bad())?;
                mode::settle_units(ms).map_err(|_| bad())?;
                cfg.min_hiss_ms = Some(ms);
            }
            _ => return Err(bad()),
        }
    }
    Ok(cfg)
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
        s += &format!("min_hiss_ms = {ms}\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_mode_and_option() {
        for mode in Mode::ALL {
            let cfg = Config {
                mode: Some(mode),
                mic_boost: Some(6),
                min_hiss_ms: Some(300),
            };
            assert_eq!(parse(&render(&cfg)).unwrap(), cfg);
        }
        assert_eq!(
            parse(&render(&Config::default())).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn rejects_out_of_range_and_unknown_values() {
        assert!(parse("mic_boost = 8").is_err());
        assert!(parse("mode = \"loud\"").is_err());
        assert!(parse("mode = \"clamp\"").is_err());
        assert!(parse("min_hiss_ms = 10").is_err());
        assert!(parse("min_hiss_ms = 2010").is_err());
        assert!(parse("min_hiss_ms = 605").is_err());
        assert!(parse("min_hiss_ms = 20").is_ok());
        assert!(parse("min_hiss_ms = 2000").is_ok());
    }
}
