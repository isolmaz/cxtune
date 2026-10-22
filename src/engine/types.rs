//! What goes into the engine and what comes out of it.

use std::fmt;

use crate::Error;
use crate::capture::Level;
use crate::config::Config;
use crate::mode::{Mode, State};
use crate::patches::{
    DSP_AGC, DSP_NR, DSP_SIDETONE, MIC_MIN, ROM_ID, SIDETONE_DEFAULT, SURROUND_LEVEL_DEFAULT,
    sidetone_db,
};

/// A request to the engine. Every setting command changes the saved settings and applies them to
/// the dongle, now or as soon as it is connected; commands that arrive in quick succession are
/// merged and only the last value is written. A value outside its range is rejected with an
/// [`ErrorKind::Invalid`] error and changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    SetMode(Mode),
    /// Lowest-noise soft-start time in ms (20-2000 in steps of 10); used while the mode is lowest
    /// noise.
    SetMinHissMs(u16),
    /// Balanced stop decay time in ms (100-2000 in steps of 10); used while the mode is balanced.
    SetBalancedDecayMs(u16),
    /// Analog mic boost step 0-7.
    SetMicBoost(u8),
    SetNoiseReduction(bool),
    SetAgc(bool),
    SetSidetone(bool),
    /// Sidetone gain register, 1-4096, in 1/256 dB: 947 is +3.70 dB (the vendor default) and
    /// 4096 is +16 dB. Every value amplifies; there is no quiet end, so sidetone off is the only
    /// silent setting. `patches::sidetone_register` converts from dB.
    SetSidetoneLevel(u16),
    /// Virtual surround on or off.
    SetSurround(bool),
    /// Virtual surround level 1-10.
    SetSurroundLevel(u8),
    /// A request for the Test tab: the microphone level, measurements and the output test helpers.
    /// Not a setting: nothing is saved. See [`TestRequest`].
    Test(TestRequest),
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
const SETTLE_TIME: &str = "soft-start time";
const DECAY_TIME: &str = "decay time";
const MIC_BOOST: &str = "mic boost";
const NOISE_REDUCTION: &str = "noise reduction";
const AGC: &str = "AGC";
const SIDETONE: &str = "sidetone";
const SIDETONE_LEVEL: &str = "sidetone level";
const SURROUND: &str = "virtual surround";
const SURROUND_LEVEL: &str = "virtual surround level";

/// `cfg` without the settings that [`Readback::differences`] calls `names`: those are left as
/// the device has them.
pub(super) fn without(cfg: &Config, names: &[&str]) -> Config {
    let mut cfg = cfg.clone();
    for name in names {
        match *name {
            MODE => cfg.mode = None,
            SETTLE_TIME => cfg.min_hiss_ms = None,
            DECAY_TIME => cfg.balanced_decay_ms = None,
            MIC_BOOST => cfg.mic_boost = None,
            NOISE_REDUCTION => cfg.mic_nr = None,
            AGC => cfg.mic_agc = None,
            SIDETONE => cfg.sidetone = None,
            SIDETONE_LEVEL => cfg.sidetone_level = None,
            SURROUND => cfg.surround = None,
            SURROUND_LEVEL => cfg.surround_level = None,
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
    /// The lowest-noise soft-start time while that routine is installed.
    pub min_hiss_ms: Option<u16>,
    /// The balanced stop decay time while that routine is installed.
    pub balanced_decay_ms: Option<u16>,
    /// `None` if the mic control byte holds a value outside the boost range.
    pub mic_boost: Option<u8>,
    pub noise_reduction: bool,
    pub agc: bool,
    pub sidetone: bool,
    /// The sidetone gain register, in 1/256 dB (`patches::sidetone_db` converts to dB).
    pub sidetone_level: u16,
    pub surround: bool,
    /// `None` if the level byte holds a level cxtune does not set (the ROM clamps it).
    pub surround_level: Option<u8>,
}

impl Readback {
    pub fn of(state: &State) -> Readback {
        Readback {
            firmware_supported: state.rom == ROM_ID,
            mode: state.mode(),
            min_hiss_ms: state.settle_ms(),
            balanced_decay_ms: state.decay_ms(),
            mic_boost: state.mic_boost(),
            noise_reduction: state.dsp_enable & DSP_NR != 0,
            agc: state.dsp_enable & DSP_AGC != 0,
            sidetone: state.dsp_enable & DSP_SIDETONE != 0,
            sidetone_level: state.sidetone_level,
            surround: state.surround_on(),
            surround_level: state.surround_level(),
        }
    }

    /// Whether this is what the dongle holds right after it starts: the vendor patch (default
    /// mode), mic control `0x3C` (boost 4), noise reduction on, AGC and sidetone off, the default
    /// sidetone level, virtual surround off at its default level.
    pub(super) fn is_vendor_boot(&self) -> bool {
        *self
            == Readback {
                firmware_supported: true,
                mode: Some(Mode::Stock),
                min_hiss_ms: None,
                balanced_decay_ms: None,
                mic_boost: Some(0x3C - MIC_MIN),
                noise_reduction: true,
                agc: false,
                sidetone: false,
                sidetone_level: SIDETONE_DEFAULT,
                surround: false,
                surround_level: Some(SURROUND_LEVEL_DEFAULT),
            }
    }

    /// The chosen settings that the device does not hold. Settings that are not chosen are left
    /// as the device has them, and the soft-start time and the decay time only count while the
    /// chosen mode is the one whose routine they belong to.
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
            DECAY_TIME,
            cfg.mode == Some(Mode::Balanced)
                && cfg
                    .balanced_decay_ms
                    .is_some_and(|ms| self.balanced_decay_ms != Some(ms)),
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
        check(SURROUND, cfg.surround.is_some_and(|on| self.surround != on));
        check(
            SURROUND_LEVEL,
            cfg.surround_level
                .is_some_and(|level| self.surround_level != Some(level)),
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
        match (self.mode, self.min_hiss_ms, self.balanced_decay_ms) {
            (Some(Mode::MinHiss), Some(ms), _) => {
                write!(f, "mode lowest-noise (soft-start {ms} ms)")?
            }
            (Some(Mode::Balanced), _, Some(ms)) => write!(f, "mode balanced (decay {ms} ms)")?,
            (Some(mode), _, _) => write!(f, "mode {}", mode.name())?,
            (None, _, _) => write!(f, "mode unrecognised")?,
        }
        match self.mic_boost {
            Some(n) => write!(f, ", mic boost {n}")?,
            None => write!(f, ", mic boost outside 0-7")?,
        }
        write!(
            f,
            ", noise reduction {}, AGC {}, sidetone {} ({:+.2} dB), surround {}",
            on_off(self.noise_reduction),
            on_off(self.agc),
            on_off(self.sidetone),
            sidetone_db(self.sidetone_level),
            on_off(self.surround)
        )?;
        match self.surround_level {
            Some(level) => write!(f, " (level {level})"),
            None => write!(f, " (level not set by cxtune)"),
        }
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
    /// What the Test tab has running.
    pub test: TestState,
}

impl Snapshot {
    /// True if the dongle was read and holds every chosen setting.
    pub fn in_sync(&self) -> bool {
        self.device
            .as_ref()
            .is_some_and(|device| device.differences(&self.settings).is_empty())
    }
}

/// A request for the Test tab. The microphone is opened only while a request needs it, and
/// closed again by the engine, which then waits for the output to settle into its idle state
/// again (see `docs/engine.md`). A request that needs the output at rest or the microphone for
/// itself ends the level meter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestRequest {
    /// Opens the microphone and reports its level every 50 ms ([`TestEvent::Level`]), or closes it
    /// again. A running test keeps the microphone open until it ends.
    Meter(bool),
    /// Measures the noise floor: the median of the 50 ms levels over `seconds` (1-60).
    NoiseFloor { seconds: u8 },
    /// Records `seconds` (1-30) into memory for [`TestRequest::PlayRecording`].
    Record { seconds: u8 },
    /// Plays the last recording through the dongle's output, at most as loud as the peak limit.
    PlayRecording,
    /// Measures the noise floor for `seconds` (1-30) with noise reduction on, then for as long with
    /// it off, and puts the setting back.
    NoiseReductionAb { seconds: u8 },
    /// Waits until the output is idle, then plays a short, low tone so that the click at its start
    /// and its end can be judged.
    ClickTest,
    /// Closes the microphone and waits until the output is idle, so that the noise floor of the
    /// idle output can be listened to. No stream is open afterwards.
    NoiseListen,
    /// Ends the running test, puts back what it changed and closes the microphone unless the meter
    /// is on.
    Cancel,
}

impl TestRequest {
    /// The longest measurement or recording that may be asked for, in seconds.
    pub const MAX_NOISE_FLOOR_SECONDS: u8 = 60;
    pub const MAX_RECORD_SECONDS: u8 = 30;
    pub const MAX_AB_SECONDS: u8 = 30;
}

/// The tests that run for a while. At most one runs at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestKind {
    NoiseFloor,
    Record,
    PlayRecording,
    NoiseReductionAb,
    ClickTest,
    NoiseListen,
}

/// What the Test tab has running.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestState {
    /// The level meter is on.
    pub meter: bool,
    /// The test that is running, if any.
    pub running: Option<TestKind>,
    /// A recording is in memory.
    pub has_recording: bool,
}

/// Where a running test is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestStep {
    /// The microphone was just opened; what it delivers while the codec starts up is left out
    /// (digital silence, a click and, when it starts from full idle in balanced mode, a stretch
    /// of higher levels).
    WarmingUp,
    /// Measuring the noise floor.
    Measuring,
    Recording,
    /// Noise reduction is on and the noise floor is measured.
    MeasuringWithNoiseReduction,
    /// Noise reduction was switched off; the level is left to settle.
    SwitchingNoiseReduction,
    MeasuringWithoutNoiseReduction,
    /// Waiting for the output to reach its idle state.
    WaitingForIdle,
    /// The output is idle; a moment of rest before the sound.
    Resting,
    Playing,
}

/// A step of a running test and the time that is left in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestProgress {
    pub kind: TestKind,
    pub step: TestStep,
    /// Milliseconds until the step ends, if it ends by itself at a known time.
    pub remaining_ms: Option<u32>,
}

/// How a test ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TestOutcome {
    NoiseFloor {
        dbfs: f32,
    },
    Recorded {
        seconds: f32,
        rms_dbfs: f32,
        peak_dbfs: f32,
    },
    Played,
    NoiseReductionAb {
        with_dbfs: f32,
        without_dbfs: f32,
        /// The noise floor without noise reduction minus the one with it.
        reduction_db: f32,
    },
    /// The tone was played. `waited_ms` is how long the output took to reach idle first.
    ClickTest {
        waited_ms: u32,
    },
    /// The output is idle and no stream is open.
    NoiseListen {
        waited_ms: u32,
    },
    Cancelled(TestKind),
}

/// What the Test tab gets besides the state.
#[derive(Clone, Debug, PartialEq)]
pub enum TestEvent {
    /// The level of the last 50 ms block, while the microphone is open for the meter.
    Level(Level),
    /// A test moved to another step, or its countdown moved on (every 100 ms).
    Progress(TestProgress),
    Done(TestOutcome),
    /// A request that could not be run, or a test that broke off. Not a problem with the settings:
    /// [`Snapshot::error`] is not touched.
    Failed {
        kind: Option<TestKind>,
        failure: Failure,
    },
}

/// What the engine reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A message for the Test tab.
    Test(TestEvent),
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
                balanced_decay_ms: Some(800),
                mic_boost: Some(4),
                noise_reduction: true,
                agc: false,
                sidetone: false,
                sidetone_level: 947,
                surround: false,
                surround_level: Some(4),
            }
        );
        assert_eq!(readback(Mode::MinHiss).min_hiss_ms, Some(600));
        assert_eq!(readback(Mode::MinHiss).balanced_decay_ms, None);

        let dev = FakeDevice::in_mode(Mode::Quiet);
        dev.poke(0x1151, &[0x90]);
        dev.poke(0x1276, &[0x00, 0x02]);
        dev.poke(0x1017, &[0x3F]);
        dev.poke(0x1152, &[0x08]);
        dev.poke(0x10D3, &[0x07]);
        let r = Readback::of(&read_state(&mut dev.clone()).unwrap());
        assert_eq!(r.mode, Some(Mode::Quiet));
        assert_eq!(r.mic_boost, Some(7));
        assert!(!r.noise_reduction && r.agc && r.sidetone);
        assert_eq!(r.sidetone_level, 0x0200);
        assert!(r.surround);
        assert_eq!(r.surround_level, Some(7));
    }

    #[test]
    fn a_surround_level_cxtune_does_not_set_is_not_reported_as_a_level() {
        for byte in [0x00, 0x0B, 0x0F, 0x24, 0x84] {
            let dev = FakeDevice::stock();
            dev.poke(0x10D3, &[byte]);
            let r = Readback::of(&read_state(&mut dev.clone()).unwrap());
            assert_eq!(r.surround_level, None, "{byte:#04x}");
            assert!(r.to_string().ends_with("(level not set by cxtune)"), "{r}");
        }
        // the variant bit does not change the level
        let dev = FakeDevice::stock();
        dev.poke(0x10D3, &[0x14]);
        let r = Readback::of(&read_state(&mut dev.clone()).unwrap());
        assert_eq!(r.surround_level, Some(4));
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
                "soft-start time",
                Readback {
                    min_hiss_ms: Some(600),
                    ..boot.clone()
                },
            ),
            (
                "decay time",
                Readback {
                    balanced_decay_ms: Some(800),
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
                "surround",
                Readback {
                    surround: true,
                    ..boot.clone()
                },
            ),
            (
                "surround level",
                Readback {
                    surround_level: Some(5),
                    ..boot.clone()
                },
            ),
            (
                "unknown surround level",
                Readback {
                    surround_level: None,
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
            // the soft-start time does not count outside lowest noise
            min_hiss_ms: Some(300),
            balanced_decay_ms: Some(800),
            surround: Some(false),
            surround_level: Some(4),
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
            (
                Config {
                    balanced_decay_ms: Some(500),
                    ..matching.clone()
                },
                "decay time",
            ),
            (
                Config {
                    surround: Some(true),
                    ..matching.clone()
                },
                "virtual surround",
            ),
            (
                Config {
                    surround_level: Some(5),
                    ..matching.clone()
                },
                "virtual surround level",
            ),
        ] {
            assert_eq!(r.differences(&cfg), [name], "{name}");
        }
    }

    #[test]
    fn a_surround_level_that_the_dongle_holds_outside_the_range_differs_from_any_chosen_level() {
        let dev = FakeDevice::stock();
        dev.poke(0x10D3, &[0x0B]);
        let r = Readback::of(&read_state(&mut dev.clone()).unwrap());
        let cfg = Config {
            surround_level: Some(4),
            ..Config::default()
        };
        assert_eq!(r.differences(&cfg), ["virtual surround level"]);
    }

    #[test]
    fn settings_can_be_left_out_by_the_names_the_differences_use() {
        let all = Config {
            mode: Some(Mode::MinHiss),
            mic_boost: Some(7),
            min_hiss_ms: Some(300),
            balanced_decay_ms: Some(500),
            mic_nr: Some(false),
            mic_agc: Some(true),
            sidetone: Some(true),
            sidetone_level: Some(100),
            surround: Some(true),
            surround_level: Some(9),
        };
        // a dongle that differs in everything it can: the decay time is not compared outside
        // balanced mode
        let names = readback(Mode::Quiet).differences(&all);
        assert_eq!(names.len(), 9, "{names:?}");
        assert!(!names.contains(&"decay time"));
        assert_eq!(
            without(&all, &names),
            Config {
                balanced_decay_ms: Some(500),
                ..Config::default()
            }
        );
        for name in &names {
            let rest = without(&all, &[name]);
            assert_ne!(rest, all, "{name}");
            // the one that was left out no longer differs (and without the mode the soft-start
            // time is not compared either)
            let left = readback(Mode::Quiet).differences(&rest);
            assert!(!left.contains(name), "{name}");
            let gone = if *name == "mode" { 2 } else { 1 };
            assert_eq!(left.len(), names.len() - gone, "{name}");
        }
        assert_eq!(without(&all, &["no such setting"]), all);
        assert_eq!(
            without(&all, &["decay time"]).balanced_decay_ms,
            None,
            "the decay time can be left out by its name"
        );
    }

    #[test]
    fn the_soft_start_time_counts_in_lowest_noise() {
        let r = readback(Mode::MinHiss);
        let cfg = |ms| Config {
            mode: Some(Mode::MinHiss),
            min_hiss_ms: Some(ms),
            ..Config::default()
        };
        assert!(r.differences(&cfg(600)).is_empty());
        assert_eq!(r.differences(&cfg(300)), ["soft-start time"]);
        // a device that is not in lowest noise differs in the mode, and in the soft-start time
        assert_eq!(
            readback(Mode::Quiet).differences(&cfg(600)),
            ["mode", "soft-start time"]
        );
    }

    #[test]
    fn the_decay_time_counts_in_balanced() {
        let r = readback(Mode::Balanced);
        let cfg = |ms| Config {
            mode: Some(Mode::Balanced),
            balanced_decay_ms: Some(ms),
            ..Config::default()
        };
        assert!(r.differences(&cfg(800)).is_empty());
        assert_eq!(r.differences(&cfg(300)), ["decay time"]);
        assert_eq!(
            readback(Mode::Quiet).differences(&cfg(800)),
            ["mode", "decay time"]
        );
        // the lowest-noise soft-start time does not count in balanced mode
        let other = Config {
            mode: Some(Mode::Balanced),
            min_hiss_ms: Some(300),
            ..Config::default()
        };
        assert!(r.differences(&other).is_empty());
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
            test: TestState::default(),
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
            "mode balanced (decay 800 ms), mic boost 4, noise reduction on, AGC off, sidetone off \
             (+3.70 dB), surround off (level 4)"
        );
        assert_eq!(
            readback(Mode::Stock).to_string(),
            "mode default, mic boost 4, noise reduction on, AGC off, sidetone off (+3.70 dB), \
             surround off (level 4)"
        );
        assert!(
            readback(Mode::MinHiss)
                .to_string()
                .starts_with("mode lowest-noise (soft-start 600 ms)")
        );
    }
}
