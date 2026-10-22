//! Settings in `%APPDATA%\cxtune\config.toml`: the selected profile and mic boost.
//! The file uses two `key = value` lines (a TOML subset).

use std::path::PathBuf;

use crate::profile::Profile;
use crate::{Error, Result};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub profile: Option<Profile>,
    /// Mic boost step 0..=7; `None` leaves $1017 untouched.
    pub mic_boost: Option<u8>,
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
            "profile" => cfg.profile = Some(Profile::parse(value).ok_or_else(bad)?),
            "mic_boost" => {
                let n: u8 = value.parse().map_err(|_| bad())?;
                if n > 7 {
                    return Err(bad());
                }
                cfg.mic_boost = Some(n);
            }
            _ => return Err(bad()),
        }
    }
    Ok(cfg)
}

fn render(cfg: &Config) -> String {
    let mut s = String::new();
    if let Some(p) = cfg.profile {
        s += &format!("profile = \"{}\"\n", p.name());
    }
    if let Some(n) = cfg.mic_boost {
        s += &format!("mic_boost = {n}\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_out_of_range_boost() {
        let cfg = Config {
            profile: Some(Profile::DeepIdleClamp),
            mic_boost: Some(6),
        };
        assert_eq!(parse(&render(&cfg)).unwrap(), cfg);
        assert!(parse("mic_boost = 8").is_err());
        assert!(parse("profile = \"loud\"").is_err());
    }
}
