//! What goes into the engine and what comes out of it.

use std::fmt;

use crate::Error;
use crate::config::Config;
use crate::mode::{Mode, State};
use crate::patches::{DSP_AGC, DSP_NR, DSP_SIDETONE, MIC_MIN, ROM_ID, SIDETONE_DEFAULT};

/// A request to the engine. Every setting command changes the saved settings and applies them to
/// the dongle, now or as soon as it is connected; commands that arrive in quick succession are
/// merged and only the last value is written. A value outside its range is rejected with an
/// [`ErrorKind::Invalid`] error and changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    SetMode(Mode),
    /// Min-hiss settle time in ms (20-2000 in steps of 10); used while the mode is min-hiss.
    SetMinHissMs(u16),
    /// Analog mic boost step 0-7.
    SetMicBoost(u8),
    SetNoiseReduction(bool),
    SetAgc(bool),
    SetSidetone(bool),
    /// Sidetone gain register, 1-4096.
    SetSidetoneLevel(u16),
    /// Applies the saved settings now, whether or not the dongle seems to hold them, and starts
    /// over after earlier failures.
    Reapply,
    /// Reads the dongle now and publishes what it holds. Writes nothing.
    Refresh,
}

/// Where the engine stands with the dongle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The dongle is not connected.
    Disconnected,
    /// The dongle is connected and left alone while its firmware loads the patch, or after the
    /// computer woke up.
    Waiting,
    /// The engine is reading or writing the dongle.
    Applying,
    /// The dongle is connected and the engine is not busy with it.
    Connected,
    /// The engine has stopped working after an internal error. [`Snapshot::error`] says why;
    /// nothing is applied any more.
    Stopped,
}

impl Phase {
    /// True while the engine is working on the dongle.
    pub fn is_busy(self) -> bool {
        self == Phase::Applying
    }
}

const MODE: &str = "mode";
const SETTLE_TIME: &str = "min-hiss settle time";
const MIC_BOOST: &str = "mic boost";
const NOISE_REDUCTION: &str = "noise reduction";
const AGC: &str = "AGC";
const SIDETONE: &str = "sidetone";
const SIDETONE_LEVEL: &str = "sidetone level";

/// `cfg` without the settings that [`Readback::differences`] calls `names`: those are left as
/// the device has them.
pub(super) fn without(cfg: &Config, names: &[&str]) -> Config {
    let mut cfg = cfg.clone();
    for name in names {
        match *name {
            MODE => cfg.mode = None,
            SETTLE_TIME => cfg.min_hiss_ms = None,
            MIC_BOOST => cfg.mic_boost = None,
            NOISE_REDUCTION => cfg.mic_nr = None,
            AGC => cfg.mic_agc = None,
            SIDETONE => cfg.sidetone = None,
            SIDETONE_LEVEL => cfg.sidetone_level = None,
            _ => {}
        }
    }
    cfg
}

/// What the dongle holds, read back from its memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Readback {
    /// False if the chip or firmware is not the one cxtune was written for. Nothing is written
    /// to such a device.
    pub firmware_supported: bool,
    /// `None` for a half-changed or unrecognised state, or an unsupported firmware.
    pub mode: Option<Mode>,
    /// The min-hiss settle time while that routine is installed.
    pub min_hiss_ms: Option<u16>,
    /// `None` if the mic control byte holds a value outside the boost range.
    pub mic_boost: Option<u8>,
    pub noise_reduction: bool,
    pub agc: bool,
    pub sidetone: bool,
    pub sidetone_level: u16,
}

impl Readback {
    pub fn of(state: &State) -> Readback {
        Readback {
            firmware_supported: state.rom == ROM_ID,
            mode: state.mode(),
            min_hiss_ms: state.settle_ms(),
            mic_boost: state.mic_boost(),
            noise_reduction: state.dsp_enable & DSP_NR != 0,
            agc: state.dsp_enable & DSP_AGC != 0,
            sidetone: state.dsp_enable & DSP_SIDETONE != 0,
            sidetone_level: state.sidetone_level,
        }
    }

    /// Whether this is what the dongle holds right after it starts: the vendor patch (stock
    /// mode), mic control `0x3C` (boost 4), noise reduction on, AGC and sidetone off, the default
    /// sidetone level.
    pub(super) fn is_vendor_boot(&self) -> bool {
        *self
            == Readback {
                firmware_supported: true,
                mode: Some(Mode::Stock),
                min_hiss_ms: None,
                mic_boost: Some(0x3C - MIC_MIN),
                noise_reduction: true,
                agc: false,
                sidetone: false,
                sidetone_level: SIDETONE_DEFAULT,
            }
    }

    /// The chosen settings that the device does not hold. Settings that are not chosen are left
    /// as the device has them, and the settle time only counts while the chosen mode is min-hiss.
    pub fn differences(&self, cfg: &Config) -> Vec<&'static str> {
        let mut differs = Vec::new();
        let mut check = |name, differs_from_saved: bool| {
            if differs_from_saved {
                differs.push(name);
            }
        };
        check(MODE, cfg.mode.is_some_and(|m| self.mode != Some(m)));
        check(
            SETTLE_TIME,
            cfg.mode == Some(Mode::MinHiss)
                && cfg
                    .min_hiss_ms
                    .is_some_and(|ms| self.min_hiss_ms != Some(ms)),
        );
        check(
            MIC_BOOST,
            cfg.mic_boost.is_some_and(|n| self.mic_boost != Some(n)),
        );
        check(
            NOISE_REDUCTION,
            cfg.mic_nr.is_some_and(|on| self.noise_reduction != on),
        );
        check(AGC, cfg.mic_agc.is_some_and(|on| self.agc != on));
        check(SIDETONE, cfg.sidetone.is_some_and(|on| self.sidetone != on));
        check(
            SIDETONE_LEVEL,
            cfg.sidetone_level
                .is_some_and(|level| self.sidetone_level != level),
        );
        differs
    }
}

impl fmt::Display for Readback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.firmware_supported {
            return write!(f, "unsupported firmware (nothing is written to it)");
        }
        let on_off = |on: bool| if on { "on" } else { "off" };
        match (self.mode, self.min_hiss_ms) {
            (Some(Mode::MinHiss), Some(ms)) => write!(f, "mode min-hiss (settle {ms} ms)")?,
            (Some(mode), _) => write!(f, "mode {}", mode.name())?,
            (None, _) => write!(f, "mode unrecognised")?,
        }
        match self.mic_boost {
            Some(n) => write!(f, ", mic boost {n}")?,
            None => write!(f, ", mic boost outside 0-7")?,
        }
        write!(
            f,
            ", noise reduction {}, AGC {}, sidetone {} (level {})",
            on_off(self.noise_reduction),
            on_off(self.agc),
            on_off(self.sidetone),
            self.sidetone_level
        )
    }
}

/// The kind of a problem, as far as a user can act on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Talking to the dongle failed: the cable, a USB reset, another program using the
    /// interface. The engine tries again.
    Communication,
    /// The silent playback stream that a mode change needs could not be used: another program
    /// holds the output, or the dongle has no playback endpoint. The engine tries again.
    Audio,
    /// A safety check refused to write: the dongle holds something cxtune does not recognise
    /// (another tool wrote to it, or its firmware is not supported). Nothing was written for
    /// the refused setting. Replugging the dongle restores the vendor patch.
    Refused,
    /// A write did not stick, or the dongle did not hold the saved settings after they were
    /// applied.
    Verify,
    /// The settings file could not be written.
    Config,
    /// A command carried a value outside its range.
    Invalid,
    /// Something else keeps changing a setting on the dongle. The engine corrected it a few
    /// times, then stopped correcting that setting.
    Drift,
    /// The dongle or its driver does not answer a request (a stuck request never returns until
    /// the dongle is replugged). Clears when the request returns.
    Unresponsive,
    /// The engine stopped after an internal error.
    Fatal,
}

impl ErrorKind {
    pub fn of(error: &Error) -> ErrorKind {
        match error {
            Error::Device(_) | Error::Protocol(_) | Error::Busy(_) => ErrorKind::Communication,
            Error::Audio(_) => ErrorKind::Audio,
            Error::Refused(_) => ErrorKind::Refused,
            Error::Readback { .. } => ErrorKind::Verify,
            Error::Config(_) => ErrorKind::Config,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::Communication => "communication",
            ErrorKind::Audio => "audio",
            ErrorKind::Refused => "refused",
            ErrorKind::Verify => "verify",
            ErrorKind::Config => "config",
            ErrorKind::Invalid => "invalid",
            ErrorKind::Drift => "drift",
            ErrorKind::Unresponsive => "unresponsive",
            ErrorKind::Fatal => "fatal",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A problem, with a message for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: ErrorKind,
    pub message: String,
}

impl From<&Error> for Failure {
    fn from(error: &Error) -> Failure {
        Failure {
            kind: ErrorKind::of(error),
            message: error.to_string(),
        }
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Failure {
        Failure::from(&error)
    }
}

/// Everything a user interface shows, as of one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub phase: Phase,
    /// The settings the user chose: what the engine keeps the dongle at. They are written to the
    /// settings file shortly after each change; [`Snapshot::settings_saved`] says whether that
    /// has happened.
    pub settings: Config,
    /// True when the settings file holds `settings`. False between a change and the write, and
    /// while the write keeps failing ([`Snapshot::save_error`]); the engine keeps trying.
    pub settings_saved: bool,
    /// Why the settings file could not be written, until a write succeeds.
    pub save_error: Option<Failure>,
    /// What the dongle held when it was last read; `None` while it is disconnected or has not
    /// been read since it was connected.
    pub device: Option<Readback>,
    /// The problem with the dongle that is still open: cleared by the next successful pass.
    pub error: Option<Failure>,
}

impl Snapshot {
    /// True if the dongle was read and holds every chosen setting.
    pub fn in_sync(&self) -> bool {
        self.device
            .as_ref()
            .is_some_and(|device| device.differences(&self.settings).is_empty())
    }
}

/// What the engine reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The state changed. Sent once per change, never twice with the same content.
    State(Snapshot),
    /// Something to be aware of that does not stop anything, such as a stream event near a DSP
    /// reload request.
    Warning(String),
    /// A problem, sent when it first appears or changes; a problem that persists through
    /// retries is not sent again. [`Snapshot::error`] holds it as long as it is open.
    Error(Failure),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeDevice;
    use crate::mode::read_state;

    fn readback(mode: Mode) -> Readback {
        Readback::of(&read_state(&mut FakeDevice::in_mode(mode)).unwrap())
    }

    #[test]
    fn a_readback_holds_every_managed_setting() {
        let r = readback(Mode::Balanced);
        assert_eq!(
            r,
            Readback {
                firmware_supported: true,
                mode: Some(Mode::Balanced),
                min_hiss_ms: None,
                mic_boost: Some(4),
                noise_reduction: true,
                agc: false,
                sidetone: false,
                sidetone_level: 947,
            }
        );
        assert_eq!(readback(Mode::MinHiss).min_hiss_ms, Some(600));

        let dev = FakeDevice::in_mode(Mode::Quiet);
        dev.poke(0x1151, &[0x90]);
        dev.poke(0x1276, &[0x00, 0x02]);
        dev.poke(0x1017, &[0x3F]);
        let r = Readback::of(&read_state(&mut dev.clone()).unwrap());
        assert_eq!(r.mode, Some(Mode::Quiet));
        assert_eq!(r.mic_boost, Some(7));
        assert!(!r.noise_reduction && r.agc && r.sidetone);
        assert_eq!(r.sidetone_level, 0x0200);
    }

    #[test]
    fn a_dongle_that_has_just_started_is_recognised_by_every_value_it_holds() {
        let boot = readback(Mode::Stock);
        assert!(boot.is_vendor_boot());

        for (what, changed) in [
            (
                "mode",
                Readback {
                    mode: Some(Mode::Quiet),
                    ..boot.clone()
                },
            ),
            (
                "unknown mode",
                Readback {
                    mode: None,
                    ..boot.clone()
                },
            ),
            (
                "mic boost",
                Readback {
                    mic_boost: Some(5),
                    ..boot.clone()
                },
            ),
            (
                "unknown mic boost",
                Readback {
                    mic_boost: None,
                    ..boot.clone()
                },
            ),
            (
                "noise reduction",
                Readback {
                    noise_reduction: false,
                    ..boot.clone()
                },
            ),
            (
                "AGC",
                Readback {
                    agc: true,
                    ..boot.clone()
                },
            ),
            (
                "sidetone",
                Readback {
                    sidetone: true,
                    ..boot.clone()
                },
            ),
            (
                "sidetone level",
                Readback {
                    sidetone_level: 948,
                    ..boot.clone()
                },
            ),
            (
                "firmware",
                Readback {
                    firmware_supported: false,
                    ..boot.clone()
                },
            ),
        ] {
            assert!(!changed.is_vendor_boot(), "{what}");
        }
        assert!(!readback(Mode::Balanced).is_vendor_boot());
        assert!(!readback(Mode::MinHiss).is_vendor_boot());
    }

    #[test]
    fn an_unsupported_firmware_has_no_mode() {
        let dev = FakeDevice::stock();
        dev.poke(0x1001, &[0x16]);
        let r = Readback::of(&read_state(&mut dev.clone()).unwrap());
        assert!(!r.firmware_supported);
        assert_eq!(r.mode, None);
        assert!(r.to_string().contains("unsupported firmware"));
    }

    #[test]
    fn only_saved_settings_can_differ() {
        let r = readback(Mode::Balanced);
        assert!(r.differences(&Config::default()).is_empty());

        let matching = Config {
            mode: Some(Mode::Balanced),
            mic_boost: Some(4),
            mic_nr: Some(true),
            mic_agc: Some(false),
            sidetone: Some(false),
            sidetone_level: Some(947),
            // the settle time does not count outside min-hiss
            min_hiss_ms: Some(300),
        };
        assert!(r.differences(&matching).is_empty());

        for (cfg, name) in [
            (
                Config {
                    mode: Some(Mode::Quiet),
                    ..matching.clone()
                },
                "mode",
            ),
            (
                Config {
                    mic_boost: Some(5),
                    ..matching.clone()
                },
                "mic boost",
            ),
            (
                Config {
                    mic_nr: Some(false),
                    ..matching.clone()
                },
                "noise reduction",
            ),
            (
                Config {
                    mic_agc: Some(true),
                    ..matching.clone()
                },
                "AGC",
            ),
            (
                Config {
                    sidetone: Some(true),
                    ..matching.clone()
                },
                "sidetone",
            ),
            (
                Config {
                    sidetone_level: Some(948),
                    ..matching.clone()
                },
                "sidetone level",
            ),
        ] {
            assert_eq!(r.differences(&cfg), [name], "{name}");
        }
    }

    #[test]
    fn settings_can_be_left_out_by_the_names_the_differences_use() {
        let all = Config {
            mode: Some(Mode::MinHiss),
            mic_boost: Some(7),
            min_hiss_ms: Some(300),
            mic_nr: Some(false),
            mic_agc: Some(true),
            sidetone: Some(true),
            sidetone_level: Some(100),
        };
        // a dongle that differs in everything
        let names = readback(Mode::Quiet).differences(&all);
        assert_eq!(names.len(), 7, "{names:?}");
        assert_eq!(without(&all, &names), Config::default());
        for name in &names {
            let rest = without(&all, &[name]);
            assert_ne!(rest, all, "{name}");
            // the one that was left out no longer differs (and without the mode the settle time
            // is not compared either)
            let left = readback(Mode::Quiet).differences(&rest);
            assert!(!left.contains(name), "{name}");
            let gone = if *name == "mode" { 2 } else { 1 };
            assert_eq!(left.len(), names.len() - gone, "{name}");
        }
        assert_eq!(without(&all, &["no such setting"]), all);
    }

    #[test]
    fn the_settle_time_counts_in_min_hiss() {
        let r = readback(Mode::MinHiss);
        let cfg = |ms| Config {
            mode: Some(Mode::MinHiss),
            min_hiss_ms: Some(ms),
            ..Config::default()
        };
        assert!(r.differences(&cfg(600)).is_empty());
        assert_eq!(r.differences(&cfg(300)), ["min-hiss settle time"]);
        // a device that is not in min-hiss differs in the mode, and in the settle time
        assert_eq!(
            readback(Mode::Quiet).differences(&cfg(600)),
            ["mode", "min-hiss settle time"]
        );
    }

    #[test]
    fn a_snapshot_is_in_sync_only_when_read_and_equal() {
        let mut snapshot = Snapshot {
            phase: Phase::Connected,
            settings: Config {
                mode: Some(Mode::Balanced),
                ..Config::default()
            },
            settings_saved: true,
            save_error: None,
            device: None,
            error: None,
        };
        assert!(!snapshot.in_sync());
        snapshot.device = Some(readback(Mode::Quiet));
        assert!(!snapshot.in_sync());
        snapshot.device = Some(readback(Mode::Balanced));
        assert!(snapshot.in_sync());
    }

    #[test]
    fn errors_are_classified_by_what_the_user_can_do() {
        let cases = [
            (Error::Device("x".into()), ErrorKind::Communication),
            (Error::Protocol("x".into()), ErrorKind::Communication),
            (Error::Audio("x".into()), ErrorKind::Audio),
            (Error::Refused("x".into()), ErrorKind::Refused),
            (
                Error::Readback {
                    patch: "p".into(),
                    addr: 0x1017,
                    expected: vec![1],
                    got: vec![2],
                },
                ErrorKind::Verify,
            ),
            (Error::Config("x".into()), ErrorKind::Config),
        ];
        for (error, kind) in cases {
            let failure = Failure::from(&error);
            assert_eq!(failure.kind, kind, "{error}");
            assert_eq!(failure.message, error.to_string());
        }
    }

    #[test]
    fn the_readback_is_described_in_one_line() {
        assert_eq!(
            readback(Mode::Balanced).to_string(),
            "mode balanced, mic boost 4, noise reduction on, AGC off, sidetone off (level 947)"
        );
        assert!(
            readback(Mode::MinHiss)
                .to_string()
                .starts_with("mode min-hiss (settle 600 ms)")
        );
    }
}
