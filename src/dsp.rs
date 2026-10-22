//! Experimental microphone DSP settings: noise reduction, AGC and sidetone.
//!
//! Noise reduction and AGC are bits of the DSP enable byte (`$1151`). The ROM evaluates them
//! when a capture stream opens and when it reloads the DSP settings on a NEWC request, so a
//! change made while a capture stream is open is followed by a NEWC request and a change made
//! without one is picked up by the next stream. Sidetone is a bit of the same byte plus a gain
//! word (`$1276`); the ROM applies both within one main-loop pass, while a capture and a
//! playback stream are open. See docs/firmware-notes.md for what was measured.

use crate::Result;
use crate::config::Config;
use crate::patches::{self, MIC_STREAM, Outcome, Patch, apply_patch};
use crate::protocol::{Transport, read_mem};

/// Sets microphone noise reduction. Returns true if a byte was written.
pub fn set_noise_reduction<T: Transport + ?Sized>(t: &mut T, on: bool) -> Result<bool> {
    set_and_reload(t, &patches::noise_reduction(on))
}

/// Sets microphone AGC. Returns true if a byte was written.
pub fn set_agc<T: Transport + ?Sized>(t: &mut T, on: bool) -> Result<bool> {
    set_and_reload(t, &patches::agc(on))
}

/// Switches sidetone on or off. Returns true if a byte was written.
pub fn set_sidetone<T: Transport + ?Sized>(t: &mut T, on: bool) -> Result<bool> {
    Ok(apply_patch(t, &patches::sidetone(on))? == Outcome::Written)
}

/// Sets the sidetone gain register (`SIDETONE_MIN..=SIDETONE_MAX`, unit not measured). Returns
/// true if a byte was written.
pub fn set_sidetone_level<T: Transport + ?Sized>(t: &mut T, level: u16) -> Result<bool> {
    Ok(apply_patch(t, &patches::sidetone_level(level)?)? == Outcome::Written)
}

/// Switches sidetone on or off and returns the number of writes made. Switching it on first
/// applies `level`, so that it never starts louder than the level that was chosen.
pub fn set_sidetone_with_level<T: Transport + ?Sized>(
    t: &mut T,
    on: bool,
    level: Option<u16>,
) -> Result<usize> {
    let mut n = 0;
    if on && let Some(level) = level {
        n += usize::from(set_sidetone_level(t, level)?);
    }
    n += usize::from(set_sidetone(t, on)?);
    Ok(n)
}

/// Applies the DSP settings that `cfg` holds and returns the number of writes made. Settings
/// that are not in `cfg` are left as the device has them.
pub fn apply_config<T: Transport + ?Sized>(t: &mut T, cfg: &Config) -> Result<usize> {
    let mut n = 0;
    if let Some(on) = cfg.mic_nr {
        n += usize::from(set_noise_reduction(t, on)?);
    }
    if let Some(on) = cfg.mic_agc {
        n += usize::from(set_agc(t, on)?);
    }
    if let Some(on) = cfg.sidetone {
        n += set_sidetone_with_level(t, on, cfg.sidetone_level)?;
    } else if let Some(level) = cfg.sidetone_level {
        n += usize::from(set_sidetone_level(t, level)?);
    }
    Ok(n)
}

/// Writes `patch`; if that changed a byte and a capture stream is open, asks the ROM to reload
/// the DSP settings so the change reaches the running stream. Without an open capture stream
/// the next one applies it, and the byte holding the ROM's stream state is not touched.
fn set_and_reload<T: Transport + ?Sized>(t: &mut T, patch: &Patch) -> Result<bool> {
    if apply_patch(t, patch)? != Outcome::Written {
        return Ok(false);
    }
    if read_mem(t, MIC_STREAM, 1)?[0] != 0 {
        apply_patch(t, &patches::newc())?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::fake::FakeDevice;
    use crate::patches::{APPLIED_AGC, APPLIED_NR};

    fn dev() -> FakeDevice {
        FakeDevice::stock()
    }

    fn put(dev: &FakeDevice, f: impl FnOnce(&mut FakeDevice) -> Result<bool>) -> bool {
        f(&mut dev.clone()).unwrap()
    }

    #[test]
    fn a_change_with_the_microphone_open_is_applied_to_the_stream_at_once() {
        let d = dev();
        let _mic = d.open_mic();
        assert_eq!(d.appliers(), APPLIED_NR);

        assert!(put(&d, |t| set_noise_reduction(t, false)));
        assert_eq!(d.appliers(), 0);
        assert_eq!(d.writes(), vec![(0x1151, vec![0x00]), (0x1154, vec![0x03])]);
        assert_eq!(d.byte(0x1154), 0x02);

        assert!(put(&d, |t| set_agc(t, true)));
        assert_eq!(d.appliers(), APPLIED_AGC);
        assert!(put(&d, |t| set_noise_reduction(t, true)));
        assert_eq!(d.appliers(), APPLIED_NR | APPLIED_AGC);
        assert_eq!(d.byte(0x1151), 0x14);
        assert_eq!(d.newc_consumed(), 3);
    }

    #[test]
    fn a_change_without_a_microphone_stream_leaves_the_stream_byte_alone() {
        let d = dev();
        assert!(put(&d, |t| set_noise_reduction(t, false)));
        assert!(put(&d, |t| set_agc(t, true)));
        assert_eq!(d.writes(), vec![(0x1151, vec![0x00]), (0x1151, vec![0x10])]);
        assert_eq!(d.newc_consumed(), 0);

        // the next capture stream applies what was written
        let _mic = d.open_mic();
        assert_eq!(d.appliers(), APPLIED_AGC);
    }

    #[test]
    fn a_setting_that_already_holds_writes_nothing_and_requests_nothing() {
        let d = dev();
        let _mic = d.open_mic();
        assert!(!put(&d, |t| set_noise_reduction(t, true)));
        assert!(!put(&d, |t| set_agc(t, false)));
        assert!(!put(&d, |t| set_sidetone(t, false)));
        assert!(!put(&d, |t| set_sidetone_level(t, 0x03B3)));
        assert!(d.writes().is_empty());
    }

    #[test]
    fn sidetone_runs_only_with_both_streams_and_needs_no_reload() {
        let d = dev();
        assert!(put(&d, |t| set_sidetone(t, true)));
        assert!(!d.sidetone_running());

        let _mic = d.open_mic();
        assert!(!d.sidetone_running());
        let _playback = d.open_active_stream().unwrap();
        assert!(d.sidetone_running());

        assert!(put(&d, |t| set_sidetone(t, false)));
        assert!(!d.sidetone_running());
        assert!(put(&d, |t| set_sidetone(t, true)));
        assert!(d.sidetone_running());
        assert_eq!(d.newc_consumed(), 0);
        assert!(d.writes().iter().all(|(addr, _)| *addr == 0x1151));
    }

    #[test]
    fn sidetone_level_is_written_without_a_reload() {
        let d = dev();
        let _mic = d.open_mic();
        assert!(put(&d, |t| set_sidetone_level(t, 0x0200)));
        assert_eq!(d.writes(), vec![(0x1276, vec![0x00, 0x02])]);
        assert_eq!(d.newc_consumed(), 0);
        for bad in [0, 0x1001, 0x8000] {
            assert!(matches!(
                set_sidetone_level(&mut d.clone(), bad),
                Err(Error::Refused(_))
            ));
        }
        assert_eq!(d.writes().len(), 1);
    }

    #[test]
    fn a_stream_event_during_the_reload_is_an_error_after_the_bit_is_written() {
        let d = dev();
        let _mic = d.open_mic();
        d.flip_on_newc(0x10);
        let err = set_noise_reduction(&mut d.clone(), false).unwrap_err();
        assert!(matches!(err, Error::Readback { addr: 0x1154, .. }), "{err}");
        assert_eq!(d.byte(0x1151), 0x00);
    }

    #[test]
    fn sidetone_starts_at_the_chosen_level() {
        for (level, expected) in [
            (
                Some(0x0200),
                vec![(0x1276, vec![0x00, 0x02]), (0x1151, vec![0x84])],
            ),
            (None, vec![(0x1151, vec![0x84])]),
        ] {
            let d = dev();
            assert_eq!(
                set_sidetone_with_level(&mut d.clone(), true, level).unwrap(),
                expected.len()
            );
            assert_eq!(d.writes(), expected);
        }
        // switching it off does not touch the level
        let d = dev();
        d.poke(0x1151, &[0x84]);
        set_sidetone_with_level(&mut d.clone(), false, Some(0x0200)).unwrap();
        assert_eq!(d.writes(), vec![(0x1151, vec![0x04])]);
    }

    #[test]
    fn the_saved_settings_are_applied_and_the_rest_left_alone() {
        let d = dev();
        assert_eq!(apply_config(&mut d.clone(), &Config::default()).unwrap(), 0);
        assert!(d.writes().is_empty());

        let cfg = Config {
            mic_nr: Some(false),
            mic_agc: Some(true),
            sidetone: Some(true),
            sidetone_level: Some(0x0200),
            ..Config::default()
        };
        assert_eq!(apply_config(&mut d.clone(), &cfg).unwrap(), 4);
        assert_eq!(
            d.writes(),
            vec![
                (0x1151, vec![0x00]),
                (0x1151, vec![0x10]),
                (0x1276, vec![0x00, 0x02]),
                (0x1151, vec![0x90]),
            ]
        );
        // everything holds now: applying again writes nothing
        d.clear_writes();
        assert_eq!(apply_config(&mut d.clone(), &cfg).unwrap(), 0);
        assert!(d.writes().is_empty());

        // a level without a sidetone switch is applied; it does not switch sidetone on
        let d = dev();
        let cfg = Config {
            sidetone_level: Some(0x0100),
            ..Config::default()
        };
        assert_eq!(apply_config(&mut d.clone(), &cfg).unwrap(), 1);
        assert_eq!(d.writes(), vec![(0x1276, vec![0x00, 0x01])]);
        assert_eq!(d.byte(0x1151), 0x04);
    }

    #[test]
    fn nothing_is_written_when_the_dsp_flags_forbid_it() {
        let d = dev();
        d.poke(0x00A6, &[0x01]);
        for f in [
            |t: &mut FakeDevice| set_noise_reduction(t, false),
            |t: &mut FakeDevice| set_agc(t, true),
            |t: &mut FakeDevice| set_sidetone(t, true),
            |t: &mut FakeDevice| set_sidetone_level(t, 0x0200),
        ] {
            assert!(matches!(f(&mut d.clone()), Err(Error::Refused(_))));
        }
        assert!(d.writes().is_empty());
    }
}
