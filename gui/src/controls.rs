//! The values the controls show, and the arithmetic behind them.

use cxtune::config::Config;
use cxtune::engine::Readback;
use cxtune::mode::{Mode, SETTLE_STEP_MS};
use cxtune::patches::{
    MIC_MAX, MIC_MIN, SETTLE_DEFAULT, SETTLE_MAX, SETTLE_MIN, SIDETONE_DEFAULT, SIDETONE_MAX,
    SIDETONE_MIN,
};

use crate::ui::{Delay, Hiss, ModeIndicators, Pop};

pub const SETTLE_MIN_MS: u16 = SETTLE_MIN as u16 * SETTLE_STEP_MS;
pub const SETTLE_MAX_MS: u16 = SETTLE_MAX as u16 * SETTLE_STEP_MS;
const SETTLE_DEFAULT_MS: u16 = SETTLE_DEFAULT as u16 * SETTLE_STEP_MS;

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
/// delay is the time that the start of audio is muted, measured on the device
/// (docs/firmware-notes.md): none in stock and quiet mode, 0.59 s plus 37 ms in balanced mode,
/// and in minimum hiss the soft-start time plus the fixed 100 ms. The start pop of balanced mode
/// was not measured.
pub fn indicators(mode: Mode, settle_ms: u16) -> ModeIndicators {
    let (hiss, pop, delay) = match mode {
        Mode::Stock => (Hiss::Constant, Pop::None, Delay::None),
        Mode::Quiet => (Hiss::Some, Pop::Low, Delay::None),
        Mode::Balanced => (Hiss::Cut, Pop::NotMeasured, Delay::About600Ms),
        Mode::MinHiss => (Hiss::Cut, Pop::Loud, Delay::Settle),
    };
    ModeIndicators {
        hiss,
        pop,
        delay,
        delay_ms: i32::from(settle_ms) + 100,
    }
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
    pub boost: u8,
    pub noise_reduction: bool,
    pub agc: bool,
    pub sidetone: bool,
    pub level: u16,
}

impl Controls {
    /// What the vendor patch holds right after the dongle starts.
    pub const VENDOR: Controls = Controls {
        mode: Mode::Stock,
        settle_ms: SETTLE_DEFAULT_MS,
        boost: BOOST_STOCK,
        noise_reduction: true,
        agc: false,
        sidetone: false,
        level: SIDETONE_DEFAULT,
    };

    /// The recommended settings: Balanced mode, and the microphone settings that measured best
    /// (README, docs/firmware-notes.md). Sidetone is off because it was never listened to. The
    /// start delay and the sidetone level are left as they are.
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
            balanced_decay_ms: None,
            surround: false,
            surround_level: Some(4),
        }
    }

    #[test]
    fn the_ranges_are_the_cores() {
        assert_eq!((SETTLE_MIN_MS, SETTLE_MAX_MS), (20, 2000));
        assert_eq!(SETTLE_DEFAULT_MS, 600);
        assert_eq!(BOOST_MAX, 7);
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
                boost: 6,
                noise_reduction: false,
                agc: true,
                sidetone: true,
                level: 500,
            }
        );
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
            ..Config::default()
        };
        assert_eq!(
            Controls::effective(&settings, Some(&device(Mode::MinHiss))),
            Controls {
                mode: Mode::Quiet,
                settle_ms: 900,
                boost: 2,
                noise_reduction: true,
                agc: false,
                sidetone: false,
                level: 70,
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
            (stock.hiss, stock.pop, stock.delay),
            (Hiss::Constant, Pop::None, Delay::None)
        );
        let quiet = indicators(Mode::Quiet, 600);
        assert_eq!(
            (quiet.hiss, quiet.pop, quiet.delay),
            (Hiss::Some, Pop::Low, Delay::None)
        );
        let balanced = indicators(Mode::Balanced, 600);
        assert_eq!(
            (balanced.hiss, balanced.pop, balanced.delay),
            (Hiss::Cut, Pop::NotMeasured, Delay::About600Ms)
        );
        let min_hiss = indicators(Mode::MinHiss, 300);
        assert_eq!(
            (
                min_hiss.hiss,
                min_hiss.pop,
                min_hiss.delay,
                min_hiss.delay_ms
            ),
            (Hiss::Cut, Pop::Loud, Delay::Settle, 400)
        );
    }

    #[test]
    fn the_recommended_settings_change_five_controls_and_keep_the_other_two() {
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
        assert_eq!(recommended.level, 500);
        assert!(recommended.boost <= BOOST_MAX);
    }

    #[test]
    fn every_register_value_survives_a_trip_through_the_slider() {
        for level in SIDETONE_MIN..=SIDETONE_MAX {
            assert_eq!(level_at(f32::from(level)), level, "{level}");
        }
    }
}
