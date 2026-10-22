//! The values the controls show, and the arithmetic behind them.

use cxtune::config::Config;
use cxtune::engine::Readback;
use cxtune::mode::{Mode, SETTLE_STEP_MS, mute_delay_ms};
use cxtune::patches::{
    DECAY_DEFAULT, DECAY_MAX, DECAY_MIN, MIC_MAX, MIC_MIN, SETTLE_DEFAULT, SETTLE_MAX, SETTLE_MIN,
    SIDETONE_DEFAULT, SIDETONE_MAX, SIDETONE_MIN, SURROUND_LEVEL_DEFAULT, SURROUND_LEVEL_MAX,
    SURROUND_LEVEL_MIN,
};

use crate::ui::{Click, ModeIndicators, NoiseFloor, StartDelay};

pub const SETTLE_MIN_MS: u16 = SETTLE_MIN as u16 * SETTLE_STEP_MS;
pub const SETTLE_MAX_MS: u16 = SETTLE_MAX as u16 * SETTLE_STEP_MS;
const SETTLE_DEFAULT_MS: u16 = SETTLE_DEFAULT as u16 * SETTLE_STEP_MS;

/// The stop decay time of Balanced mode, in ms; it has the same step as the soft-start time.
pub const DECAY_MIN_MS: u16 = DECAY_MIN as u16 * SETTLE_STEP_MS;
pub const DECAY_MAX_MS: u16 = DECAY_MAX as u16 * SETTLE_STEP_MS;
const DECAY_DEFAULT_MS: u16 = DECAY_DEFAULT as u16 * SETTLE_STEP_MS;

pub const SURROUND_MIN: u8 = SURROUND_LEVEL_MIN;
pub const SURROUND_MAX: u8 = SURROUND_LEVEL_MAX;

pub const BOOST_MAX: u8 = MIC_MAX - MIC_MIN;
/// The step the vendor patch sets (`$1017 = 0x3C`).
pub const BOOST_STOCK: u8 = 4;
/// The step with the best signal-to-noise ratio in the measurements (README).
pub const BOOST_RECOMMENDED: u8 = 6;
/// Each boost step is this many dB.
pub const BOOST_STEP_DB: i32 = 6;

/// The gain of a boost step compared with the vendor's step. The gain of step 0 itself is not
/// known, so nothing is said about the absolute gain.
pub fn gain_db(step: u8) -> i32 {
    (i32::from(step) - i32::from(BOOST_STOCK)) * BOOST_STEP_DB
}

/// The mode that the recommended settings use.
pub const RECOMMENDED_MODE: Mode = Mode::Balanced;

/// What a mode costs and gives, for the indicators on its card. The words are the README's; the
/// delay is the engine's own figure for the time that the start of audio is muted
/// (`mode::mute_delay_ms`). The start pop of balanced mode was not measured.
pub fn indicators(mode: Mode, settle_ms: u16) -> ModeIndicators {
    let (noise_floor, click, start_delay) = match mode {
        Mode::Stock => (NoiseFloor::Constant, Click::None, StartDelay::None),
        Mode::Quiet => (NoiseFloor::Some, Click::Low, StartDelay::None),
        Mode::Balanced => (NoiseFloor::Cut, Click::NotMeasured, StartDelay::Fixed),
        Mode::MinHiss => (NoiseFloor::Cut, Click::Loud, StartDelay::Settle),
    };
    ModeIndicators {
        noise_floor,
        click,
        start_delay,
        start_delay_ms: mute_delay_ms(Some(mode), Some(settle_ms)) as i32,
    }
}

/// The soft-start time is used only by Lowest noise, and the stop decay time only by Balanced;
/// the engine takes a change of either at any time, but it does nothing in another mode.
pub fn soft_start_enabled(usable: bool, mode: Mode) -> bool {
    usable && mode == Mode::MinHiss
}

pub fn decay_enabled(usable: bool, mode: Mode) -> bool {
    usable && mode == Mode::Balanced
}

/// The level that switching sidetone on starts with when the user has not chosen one: 200 is
/// +0.78 dB, about 3 dB below the vendor's +3.7 dB, because the loudness of sidetone was never
/// measured and it can be loud.
pub const SIDETONE_START: u16 = 200;

/// The level to set before sidetone is switched on, if there is one. The vendor's level was
/// never chosen to be listened to in sensitive earphones, so unless the user has picked a level
/// sidetone starts at a low one.
pub fn level_before_sidetone(level: u16, chosen: bool) -> Option<u16> {
    (!chosen && level == SIDETONE_DEFAULT).then_some(SIDETONE_START)
}

/// A key press moves the sidetone slider by this much: half a dB, counting 256 to the dB as the
/// ROM's USB audio code does.
pub const LEVEL_STEP: u16 = 128;

/// Every value a control shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Controls {
    pub mode: Mode,
    pub settle_ms: u16,
    pub decay_ms: u16,
    pub boost: u8,
    pub noise_reduction: bool,
    pub agc: bool,
    pub sidetone: bool,
    pub level: u16,
    pub surround: bool,
    pub surround_level: u8,
}

impl Controls {
    /// What the vendor patch holds right after the dongle starts.
    pub const VENDOR: Controls = Controls {
        mode: Mode::Stock,
        settle_ms: SETTLE_DEFAULT_MS,
        decay_ms: DECAY_DEFAULT_MS,
        boost: BOOST_STOCK,
        noise_reduction: true,
        agc: false,
        sidetone: false,
        level: SIDETONE_DEFAULT,
        surround: false,
        surround_level: SURROUND_LEVEL_DEFAULT,
    };

    /// The recommended settings: Balanced mode, and the microphone settings that measured best
    /// (README, docs/firmware-notes.md). Sidetone is off because it was never listened to. The
    /// soft-start time, the release time, the sidetone level and the virtual surround are left as
    /// they are.
    pub fn recommended(self) -> Controls {
        Controls {
            mode: RECOMMENDED_MODE,
            boost: BOOST_RECOMMENDED,
            noise_reduction: true,
            agc: false,
            sidetone: false,
            ..self
        }
    }

    /// The setting the user chose, or else what the dongle holds, or else the vendor's value. A
    /// setting that was never chosen is left alone by the engine, so the dongle is what is true.
    pub fn effective(settings: &Config, device: Option<&Readback>) -> Controls {
        let vendor = Controls::VENDOR;
        let device = device.filter(|device| device.firmware_supported);
        Controls {
            mode: settings
                .mode
                .or(device.and_then(|d| d.mode))
                .unwrap_or(vendor.mode),
            settle_ms: settings
                .min_hiss_ms
                .or(device.and_then(|d| d.min_hiss_ms))
                .unwrap_or(vendor.settle_ms),
            decay_ms: settings
                .balanced_decay_ms
                .or(device.and_then(|d| d.balanced_decay_ms))
                .unwrap_or(vendor.decay_ms),
            boost: settings
                .mic_boost
                .or(device.and_then(|d| d.mic_boost))
                .unwrap_or(vendor.boost),
            noise_reduction: settings
                .mic_nr
                .or(device.map(|d| d.noise_reduction))
                .unwrap_or(vendor.noise_reduction),
            agc: settings
                .mic_agc
                .or(device.map(|d| d.agc))
                .unwrap_or(vendor.agc),
            sidetone: settings
                .sidetone
                .or(device.map(|d| d.sidetone))
                .unwrap_or(vendor.sidetone),
            level: settings
                .sidetone_level
                .or(device.map(|d| d.sidetone_level))
                .filter(|level| (SIDETONE_MIN..=SIDETONE_MAX).contains(level))
                .unwrap_or(vendor.level),
            surround: settings
                .surround
                .or(device.map(|d| d.surround))
                .unwrap_or(vendor.surround),
            surround_level: settings
                .surround_level
                .or(device.and_then(|d| d.surround_level))
                .filter(|level| (SURROUND_MIN..=SURROUND_MAX).contains(level))
                .unwrap_or(vendor.surround_level),
        }
    }
}

pub fn mode_index(mode: Mode) -> i32 {
    Mode::ALL.iter().position(|&m| m == mode).unwrap_or(0) as i32
}

pub fn mode_at(index: i32) -> Option<Mode> {
    usize::try_from(index)
        .ok()
        .and_then(|index| Mode::ALL.get(index).copied())
}

/// The sidetone register value at a slider value. The slider is linear in the register, so it is
/// linear in dB.
pub fn level_at(value: f32) -> u16 {
    value
        .round()
        .clamp(f32::from(SIDETONE_MIN), f32::from(SIDETONE_MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(mode: Mode) -> Readback {
        Readback {
            firmware_supported: true,
            mode: Some(mode),
            min_hiss_ms: (mode == Mode::MinHiss).then_some(300),
            mic_boost: Some(6),
            noise_reduction: false,
            agc: true,
            sidetone: true,
            sidetone_level: 500,
            balanced_decay_ms: (mode == Mode::Balanced).then_some(300),
            surround: true,
            surround_level: Some(7),
        }
    }

    #[test]
    fn the_ranges_are_the_cores() {
        assert_eq!((SETTLE_MIN_MS, SETTLE_MAX_MS), (20, 2000));
        assert_eq!(SETTLE_DEFAULT_MS, 600);
        assert_eq!(BOOST_MAX, 7);
        assert_eq!((DECAY_MIN_MS, DECAY_MAX_MS), (100, 2000));
        assert_eq!(DECAY_DEFAULT_MS, 800);
        assert_eq!((SURROUND_MIN, SURROUND_MAX), (1, 10));
        for ms in [DECAY_MIN_MS, DECAY_DEFAULT_MS, DECAY_MAX_MS] {
            assert!(cxtune::mode::decay_units(ms).is_ok(), "{ms}");
        }
        for ms in [DECAY_MIN_MS - SETTLE_STEP_MS, DECAY_MAX_MS + SETTLE_STEP_MS] {
            assert!(cxtune::mode::decay_units(ms).is_err(), "{ms}");
        }
        for ms in [SETTLE_MIN_MS, 600, SETTLE_MAX_MS] {
            assert!(cxtune::mode::settle_units(ms).is_ok(), "{ms}");
        }
        for ms in [
            SETTLE_MIN_MS - SETTLE_STEP_MS,
            SETTLE_MAX_MS + SETTLE_STEP_MS,
        ] {
            assert!(cxtune::mode::settle_units(ms).is_err(), "{ms}");
        }
        assert!((SIDETONE_MIN..=SIDETONE_MAX).contains(&SIDETONE_START));
        const { assert!(SIDETONE_START < SIDETONE_DEFAULT) };
    }

    #[test]
    fn nothing_chosen_and_no_dongle_shows_the_vendor_values() {
        assert_eq!(
            Controls::effective(&Config::default(), None),
            Controls::VENDOR
        );
    }

    #[test]
    fn a_setting_that_was_not_chosen_shows_what_the_dongle_holds() {
        let held = Controls::effective(&Config::default(), Some(&device(Mode::MinHiss)));
        assert_eq!(
            held,
            Controls {
                mode: Mode::MinHiss,
                settle_ms: 300,
                decay_ms: DECAY_DEFAULT_MS,
                boost: 6,
                noise_reduction: false,
                agc: true,
                sidetone: true,
                level: 500,
                surround: true,
                surround_level: 7,
            }
        );
        let balanced = Controls::effective(&Config::default(), Some(&device(Mode::Balanced)));
        assert_eq!(balanced.decay_ms, 300);
    }

    #[test]
    fn a_chosen_setting_wins_over_the_dongle() {
        let settings = Config {
            mode: Some(Mode::Quiet),
            min_hiss_ms: Some(900),
            mic_boost: Some(2),
            mic_nr: Some(true),
            mic_agc: Some(false),
            sidetone: Some(false),
            sidetone_level: Some(70),
            balanced_decay_ms: Some(1500),
            surround: Some(false),
            surround_level: Some(2),
        };
        assert_eq!(
            Controls::effective(&settings, Some(&device(Mode::Balanced))),
            Controls {
                mode: Mode::Quiet,
                settle_ms: 900,
                decay_ms: 1500,
                boost: 2,
                noise_reduction: true,
                agc: false,
                sidetone: false,
                level: 70,
                surround: false,
                surround_level: 2,
            }
        );
    }

    #[test]
    fn what_a_dongle_with_other_firmware_holds_is_not_shown() {
        let mut other = device(Mode::Balanced);
        other.firmware_supported = false;
        assert_eq!(
            Controls::effective(&Config::default(), Some(&other)),
            Controls::VENDOR
        );
    }

    #[test]
    fn values_the_dongle_holds_outside_the_ranges_fall_back_to_the_vendor_values() {
        let mut odd = device(Mode::Quiet);
        odd.mic_boost = None;
        odd.mode = None;
        odd.sidetone_level = 0x80B3;
        let shown = Controls::effective(&Config::default(), Some(&odd));
        assert_eq!(shown.mode, Controls::VENDOR.mode);
        assert_eq!(shown.boost, Controls::VENDOR.boost);
        assert_eq!(shown.level, Controls::VENDOR.level);
    }

    #[test]
    fn modes_are_listed_in_the_order_of_the_choices() {
        let names: Vec<_> = (0..4).map(|i| mode_at(i).unwrap().name()).collect();
        assert_eq!(names, ["default", "low-click", "balanced", "lowest-noise"]);
        for mode in Mode::ALL {
            assert_eq!(mode_at(mode_index(mode)), Some(mode));
        }
        assert_eq!(mode_at(-1), None);
        assert_eq!(mode_at(4), None);
    }

    #[test]
    fn the_sidetone_slider_covers_the_whole_register_range_and_nothing_else() {
        assert_eq!(level_at(0.0), SIDETONE_MIN);
        assert_eq!(level_at(-50.0), SIDETONE_MIN);
        assert_eq!(level_at(f32::from(SIDETONE_MAX) + 50.0), SIDETONE_MAX);
        assert_eq!(level_at(1000.4), 1000);
        assert_eq!(level_at(1000.6), 1001);
    }

    #[test]
    fn equal_steps_on_the_sidetone_slider_are_equal_steps_in_the_register() {
        let step = f32::from(LEVEL_STEP);
        let start = f32::from(SIDETONE_MIN);
        for k in 1..=31 {
            let (before, after) = (
                level_at(start + step * (k - 1) as f32),
                level_at(start + step * k as f32),
            );
            assert_eq!(after - before, LEVEL_STEP, "step {k}");
        }
    }

    #[test]
    fn sidetone_starts_low_unless_the_user_has_chosen_a_level() {
        assert_eq!(
            level_before_sidetone(SIDETONE_DEFAULT, false),
            Some(SIDETONE_START)
        );
        // a level that the user moved to, even if it is the vendor's again
        assert_eq!(level_before_sidetone(SIDETONE_DEFAULT, true), None);
        // a level other than the vendor's was chosen at some time
        assert_eq!(level_before_sidetone(70, false), None);
        assert_eq!(level_before_sidetone(SIDETONE_MAX, false), None);
    }

    #[test]
    fn the_gain_is_counted_from_the_vendor_step_in_steps_of_6_db() {
        assert_eq!(gain_db(BOOST_STOCK), 0);
        assert_eq!(gain_db(6), 12);
        assert_eq!(gain_db(7), 18);
        assert_eq!(gain_db(0), -24);
    }

    #[test]
    fn the_indicators_show_what_the_documentation_measured_and_say_what_it_did_not() {
        let stock = indicators(Mode::Stock, 600);
        assert_eq!(
            (
                stock.noise_floor,
                stock.click,
                stock.start_delay,
                stock.start_delay_ms
            ),
            (NoiseFloor::Constant, Click::None, StartDelay::None, 0)
        );
        let quiet = indicators(Mode::Quiet, 600);
        assert_eq!(
            (
                quiet.noise_floor,
                quiet.click,
                quiet.start_delay,
                quiet.start_delay_ms
            ),
            (NoiseFloor::Some, Click::Low, StartDelay::None, 0)
        );
        let balanced = indicators(Mode::Balanced, 600);
        assert_eq!(
            (
                balanced.noise_floor,
                balanced.click,
                balanced.start_delay,
                balanced.start_delay_ms
            ),
            (NoiseFloor::Cut, Click::NotMeasured, StartDelay::Fixed, 650)
        );
        let min_hiss = indicators(Mode::MinHiss, 300);
        assert_eq!(
            (
                min_hiss.noise_floor,
                min_hiss.click,
                min_hiss.start_delay,
                min_hiss.start_delay_ms
            ),
            (NoiseFloor::Cut, Click::Loud, StartDelay::Settle, 450)
        );
    }

    #[test]
    fn each_time_is_enabled_only_in_the_mode_that_uses_it_and_with_a_dongle() {
        for mode in Mode::ALL {
            assert_eq!(
                soft_start_enabled(true, mode),
                mode == Mode::MinHiss,
                "{mode:?}"
            );
            assert_eq!(
                decay_enabled(true, mode),
                mode == Mode::Balanced,
                "{mode:?}"
            );
            assert!(!soft_start_enabled(false, mode), "{mode:?}");
            assert!(!decay_enabled(false, mode), "{mode:?}");
        }
    }

    #[test]
    fn the_recommended_settings_change_five_controls_and_keep_the_others() {
        let held = Controls::effective(&Config::default(), Some(&device(Mode::MinHiss)));
        let recommended = held.recommended();
        assert_eq!(
            recommended,
            Controls {
                mode: Mode::Balanced,
                boost: 6,
                noise_reduction: true,
                agc: false,
                sidetone: false,
                ..held
            }
        );
        assert_eq!(recommended.settle_ms, 300);
        assert_eq!(recommended.decay_ms, DECAY_DEFAULT_MS);
        assert_eq!(recommended.level, 500);
        assert!(recommended.surround);
        assert_eq!(recommended.surround_level, 7);
        assert!(recommended.boost <= BOOST_MAX);
    }

    #[test]
    fn every_register_value_survives_a_trip_through_the_slider() {
        for level in SIDETONE_MIN..=SIDETONE_MAX {
            assert_eq!(level_at(f32::from(level)), level, "{level}");
        }
    }
}
