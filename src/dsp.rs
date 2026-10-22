//! Experimental DSP settings: microphone noise reduction, AGC and sidetone, and the virtual
//! surround.
//!
//! Noise reduction and AGC are bits of the DSP enable byte (`$1151`). The ROM evaluates them
//! when a capture stream opens and when it reloads the DSP settings on a NEWC request, so a
//! change made while a capture stream is open is followed by a NEWC request and a change made
//! without one is picked up by the next stream. Sidetone is a bit of the same byte plus a gain
//! word (`$1276`, in 1/256 dB, never below 0 dB); the ROM applies both within one main-loop pass,
//! while a capture and a playback stream are open. The virtual surround is a bit and a level
//! nibble of two bytes that the ROM applies at the same two moments, while a playback stream is
//! open. See docs/firmware-notes.md for what was measured.

use crate::config::Config;
use crate::patches::{
    self, DSP_ENABLE, DSP_SIDETONE, MIC_STREAM, Outcome, PLAYBACK_STREAM, Patch,
    SIDETONE_LEVEL_ADDR, SIDETONE_MAX, SIDETONE_MIN, SURROUND_ADDR, SURROUND_BIT, StreamEvent,
    apply_patch,
};
use crate::protocol::{Transport, read_mem};
use crate::{Error, Result};

/// Number of gain steps when the sidetone level changes while sidetone is on. Each step is a
/// separate guarded write, and all steps are the same size in dB.
const RAMP_STEPS: u32 = 4;

/// What a setter did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Writes made: the setting and, if a capture stream was open, the reload request (0: the
    /// device already held the value).
    pub writes: usize,
    /// A stream event happened near the reload request that followed the write. The setting is
    /// written and the reload was done; see [`StreamEvent`].
    pub stream_event: Option<StreamEvent>,
}

/// Writes made and stream events seen while applying saved settings.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub writes: usize,
    pub stream_events: Vec<StreamEvent>,
}

impl Report {
    fn add(&mut self, applied: Applied) {
        self.writes += applied.writes;
        self.stream_events.extend(applied.stream_event);
    }
}

/// What an attempt to apply several settings did, and the error that stopped it, if any. The
/// report holds the writes made and the stream events seen before the error, so a refusal does
/// not hide them.
#[derive(Debug, Default)]
pub struct Attempt {
    pub report: Report,
    pub error: Option<Error>,
}

/// Sets microphone noise reduction.
pub fn set_noise_reduction<T: Transport + ?Sized>(t: &mut T, on: bool) -> Result<Applied> {
    set_and_reload(t, &patches::noise_reduction(on), MIC_STREAM)
}

/// Sets microphone AGC.
pub fn set_agc<T: Transport + ?Sized>(t: &mut T, on: bool) -> Result<Applied> {
    set_and_reload(t, &patches::agc(on), MIC_STREAM)
}

/// Sets the virtual surround. A playback stream that is open already gets it by a reload request;
/// otherwise the next one applies it.
pub fn set_surround<T: Transport + ?Sized>(t: &mut T, on: bool) -> Result<Applied> {
    set_and_reload(t, &patches::surround(on), PLAYBACK_STREAM)
}

/// Sets the virtual surround level (`SURROUND_LEVEL_MIN..=SURROUND_LEVEL_MAX`). The ROM only uses
/// it while the surround is on, so with the surround off the byte is written and no reload is
/// requested.
pub fn set_surround_level<T: Transport + ?Sized>(t: &mut T, level: u8) -> Result<Applied> {
    let patch = patches::surround_level(level)?;
    if read_mem(t, SURROUND_ADDR, 1)?[0] & SURROUND_BIT == 0 {
        return Ok(Applied {
            writes: usize::from(apply_patch(t, &patch)? == Outcome::Written),
            stream_event: None,
        });
    }
    set_and_reload(t, &patch, PLAYBACK_STREAM)
}

/// Switches sidetone on or off. Returns true if a byte was written.
pub fn set_sidetone<T: Transport + ?Sized>(t: &mut T, on: bool) -> Result<bool> {
    Ok(apply_patch(t, &patches::sidetone(on))? == Outcome::Written)
}

/// Sets the sidetone gain register (`SIDETONE_MIN..=SIDETONE_MAX`, in 1/256 dB, see
/// [`patches::sidetone_db`]). While sidetone is on the register moves to the new value in
/// [`RAMP_STEPS`] steps of equal size in dB.
/// Returns the number of writes made.
pub fn set_sidetone_level<T: Transport + ?Sized>(t: &mut T, level: u16) -> Result<usize> {
    patches::sidetone_level(level)?;
    let on = read_mem(t, DSP_ENABLE, 1)?[0] & DSP_SIDETONE != 0;
    let current = read_mem(t, SIDETONE_LEVEL_ADDR, 2)?;
    let current = u16::from_le_bytes([current[0], current[1]]);
    let steps = if on && (SIDETONE_MIN..=SIDETONE_MAX).contains(&current) {
        ramp(current, level)
    } else {
        vec![level]
    };
    let mut writes = 0;
    for step in steps {
        writes += usize::from(apply_patch(t, &patches::sidetone_level(step)?)? == Outcome::Written);
    }
    Ok(writes)
}

/// The register values from `from` (excluded) to `to` (included): the register counts in 1/256 dB,
/// so equal steps of the register are equal steps in dB. Every value is between the two, none
/// twice in a row.
fn ramp(from: u16, to: u16) -> Vec<u16> {
    let mut steps: Vec<u16> = (1..RAMP_STEPS)
        .map(|i| {
            let gain = f64::from(from)
                + (f64::from(to) - f64::from(from)) * f64::from(i) / f64::from(RAMP_STEPS);
            (gain.round() as u16).clamp(from.min(to), from.max(to))
        })
        .collect();
    steps.push(to);
    steps.dedup();
    steps.retain(|&gain| gain != from);
    steps
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
        n += set_sidetone_level(t, level)?;
    }
    n += usize::from(set_sidetone(t, on)?);
    Ok(n)
}

/// Applies the DSP settings that `cfg` holds, stopping at the first error. Settings that are not
/// in `cfg` are left as the device has them. A saved sidetone level is written whether sidetone
/// is on or off; switching sidetone on writes it first.
pub fn apply_config<T: Transport + ?Sized>(t: &mut T, cfg: &Config) -> Attempt {
    let mut report = Report::default();
    let error = apply_settings(t, cfg, &mut report).err();
    Attempt { report, error }
}

fn apply_settings<T: Transport + ?Sized>(
    t: &mut T,
    cfg: &Config,
    report: &mut Report,
) -> Result<()> {
    if let Some(on) = cfg.mic_nr {
        report.add(set_noise_reduction(t, on)?);
    }
    if let Some(on) = cfg.mic_agc {
        report.add(set_agc(t, on)?);
    }
    if let Some(on) = cfg.surround {
        report.add(set_surround(t, on)?);
    }
    if let Some(level) = cfg.surround_level {
        report.add(set_surround_level(t, level)?);
    }
    if cfg.sidetone == Some(true) {
        report.writes += set_sidetone_with_level(t, true, cfg.sidetone_level)?;
        return Ok(());
    }
    if cfg.sidetone == Some(false) {
        report.writes += usize::from(set_sidetone(t, false)?);
    }
    if let Some(level) = cfg.sidetone_level {
        report.writes += set_sidetone_level(t, level)?;
    }
    Ok(())
}

/// Writes `patch`; if that changed a byte and the stream whose interface register is `stream` is
/// open, asks the ROM to reload the DSP settings so the change reaches the running stream.
/// Without an open stream the next one applies it, and the byte holding the ROM's stream state is
/// not touched.
fn set_and_reload<T: Transport + ?Sized>(t: &mut T, patch: &Patch, stream: u16) -> Result<Applied> {
    if apply_patch(t, patch)? != Outcome::Written {
        return Ok(Applied::default());
    }
    let mut applied = Applied {
        writes: 1,
        stream_event: None,
    };
    if read_mem(t, stream, 1)?[0] != 0 {
        match apply_patch(t, &patches::newc())? {
            Outcome::Written => applied.writes += 1,
            Outcome::StreamEvent(event) => {
                applied.writes += 1;
                applied.stream_event = Some(event);
            }
            Outcome::AlreadySet => {}
        }
    }
    Ok(applied)
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

    fn put(dev: &FakeDevice, f: impl FnOnce(&mut FakeDevice) -> Result<Applied>) -> bool {
        f(&mut dev.clone()).unwrap().writes > 0
    }

    fn set_level(dev: &FakeDevice, level: u16) -> bool {
        set_sidetone_level(&mut dev.clone(), level).unwrap() > 0
    }

    /// The levels written to `$1276`, in order.
    fn levels(dev: &FakeDevice) -> Vec<u16> {
        dev.writes()
            .into_iter()
            .filter(|(addr, _)| *addr == 0x1276)
            .map(|(_, data)| u16::from_le_bytes([data[0], data[1]]))
            .collect()
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
        assert!(!set_sidetone(&mut d.clone(), false).unwrap());
        assert!(!set_level(&d, 0x03B3));
        assert!(d.writes().is_empty());
    }

    #[test]
    fn sidetone_runs_only_with_both_streams_and_needs_no_reload() {
        let d = dev();
        assert!(set_sidetone(&mut d.clone(), true).unwrap());
        assert!(!d.sidetone_running());

        let _mic = d.open_mic();
        assert!(!d.sidetone_running());
        let _playback = d.open_active_stream().unwrap();
        assert!(d.sidetone_running());

        assert!(set_sidetone(&mut d.clone(), false).unwrap());
        assert!(!d.sidetone_running());
        assert!(set_sidetone(&mut d.clone(), true).unwrap());
        assert!(d.sidetone_running());
        assert_eq!(d.newc_consumed(), 0);
        assert!(d.writes().iter().all(|(addr, _)| *addr == 0x1151));
    }

    #[test]
    fn sidetone_level_is_written_without_a_reload() {
        let d = dev();
        let _mic = d.open_mic();
        assert!(set_level(&d, 0x0200));
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
    fn the_level_moves_in_steps_while_sidetone_is_on() {
        for (from, to) in [
            (947u16, 4096u16),
            (4096, 100),
            (1, 4096),
            (4096, 1),
            (947, 948),
        ] {
            let d = dev();
            d.poke(0x1151, &[0x84]);
            d.poke(0x1276, &from.to_le_bytes());
            assert!(set_level(&d, to));
            let steps = levels(&d);
            assert_eq!(*steps.last().unwrap(), to, "{from} -> {to}");
            assert_eq!(d.byte(0x1276), to.to_le_bytes()[0]);
            if from.abs_diff(to) > 1 {
                assert!(steps.len() > 1, "{from} -> {to}: {steps:?}");
            }
            let mut previous = from;
            for &step in &steps {
                assert!(
                    (SIDETONE_MIN..=SIDETONE_MAX).contains(&step),
                    "{from} -> {to}: {step}"
                );
                // monotonic towards the target, never past it, and no step is larger than an
                // equal share of the whole change in dB (plus rounding)
                assert_ne!(step, previous, "{from} -> {to}: {steps:?}");
                assert!(
                    (previous < to && step > previous && step <= to)
                        || (previous > to && step < previous && step >= to),
                    "{from} -> {to}: {steps:?}"
                );
                let share = from.abs_diff(to).div_ceil(RAMP_STEPS as u16) + 1;
                assert!(
                    step.abs_diff(previous) <= share,
                    "{from} -> {to}: {steps:?}"
                );
                previous = step;
            }
        }
    }

    #[test]
    fn the_steps_are_equal_in_db_not_in_ratio() {
        // 947 is +3.70 dB and 200 is +0.78 dB: the four steps are 0.73 dB each
        assert_eq!(ramp(947, 200), [760, 574, 387, 200]);
        assert_eq!(ramp(200, 947), [387, 574, 760, 947]);
        assert_eq!(ramp(1, 4096), [1025, 2049, 3072, 4096]);
        assert_eq!(ramp(4096, 1), [3072, 2049, 1025, 1]);
        // a change smaller than the number of steps has fewer steps
        assert_eq!(ramp(947, 949), [948, 949]);
        assert_eq!(ramp(947, 948), [948]);
    }

    #[test]
    fn the_level_jumps_while_sidetone_is_off_and_does_nothing_when_it_holds() {
        let d = dev();
        assert!(set_level(&d, 4096));
        assert_eq!(levels(&d), vec![4096]);
        assert!(!set_level(&d, 4096));
        assert_eq!(levels(&d), vec![4096]);
    }

    #[test]
    fn a_stream_event_near_the_reload_is_a_warning_and_not_an_error() {
        let d = dev();
        let _mic = d.open_mic();
        d.flip_on_newc(0x10);
        let applied = set_noise_reduction(&mut d.clone(), false).unwrap();
        assert_eq!(applied.writes, 2);
        assert_eq!(
            applied.stream_event,
            Some(StreamEvent {
                before: 0x02,
                after: 0x12
            })
        );
        assert_eq!(d.byte(0x1151), 0x00);
        assert_eq!(d.appliers(), 0);
    }

    #[test]
    fn a_request_the_rom_already_has_pending_is_waited_for() {
        let d = dev();
        let _mic = d.open_mic();
        // the ROM's own request is consumed a few reads after the write
        d.raise_newc(12);
        let applied = set_noise_reduction(&mut d.clone(), false).unwrap();
        assert_eq!(
            applied,
            Applied {
                writes: 1,
                stream_event: None
            }
        );
        assert_eq!(d.writes(), vec![(0x1151, vec![0x00])]);
        assert_eq!(d.byte(0x1154) & 0x01, 0);
        assert_eq!(d.appliers(), 0);
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

    /// Applies `cfg` and asserts that nothing went wrong.
    fn apply_all(d: &FakeDevice, cfg: &Config) -> Report {
        let attempt = apply_config(&mut d.clone(), cfg);
        assert!(attempt.error.is_none(), "{:?}", attempt.error);
        attempt.report
    }

    #[test]
    fn the_saved_settings_are_applied_and_the_rest_left_alone() {
        let d = dev();
        let none = apply_all(&d, &Config::default());
        assert_eq!(none, Report::default());
        assert!(d.writes().is_empty());

        let cfg = Config {
            mic_nr: Some(false),
            mic_agc: Some(true),
            sidetone: Some(true),
            sidetone_level: Some(0x0200),
            ..Config::default()
        };
        assert_eq!(apply_all(&d, &cfg).writes, 4);
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
        assert_eq!(apply_all(&d, &cfg).writes, 0);
        assert!(d.writes().is_empty());

        // a level without a sidetone switch is applied; it does not switch sidetone on
        let d = dev();
        let cfg = Config {
            sidetone_level: Some(0x0100),
            ..Config::default()
        };
        assert_eq!(apply_all(&d, &cfg).writes, 1);
        assert_eq!(d.writes(), vec![(0x1276, vec![0x00, 0x01])]);
        assert_eq!(d.byte(0x1151), 0x04);
    }

    #[test]
    fn a_saved_level_is_written_while_sidetone_is_off_and_after_it_is_switched_off() {
        let d = dev();
        let cfg = Config {
            sidetone: Some(false),
            sidetone_level: Some(0x0200),
            ..Config::default()
        };
        assert_eq!(apply_all(&d, &cfg).writes, 1);
        assert_eq!(d.writes(), vec![(0x1276, vec![0x00, 0x02])]);

        // sidetone is on at the stock level: it is switched off first, so the level jumps
        // instead of moving in steps
        let d = dev();
        d.poke(0x1151, &[0x84]);
        assert_eq!(apply_all(&d, &cfg).writes, 2);
        assert_eq!(
            d.writes(),
            vec![(0x1151, vec![0x04]), (0x1276, vec![0x00, 0x02])]
        );
    }

    #[test]
    fn a_refused_setting_keeps_the_report_of_the_ones_before_it() {
        let d = dev();
        let _mic = d.open_mic();
        d.flip_on_newc(0x10);
        // the sidetone level is outside what cxtune replaces, after two settings were applied
        d.poke(0x1276, &0x8000u16.to_le_bytes());
        let cfg = Config {
            mic_nr: Some(false),
            mic_agc: Some(true),
            sidetone_level: Some(0x0200),
            ..Config::default()
        };
        let attempt = apply_config(&mut d.clone(), &cfg);
        assert!(matches!(attempt.error, Some(Error::Refused(_))));
        assert_eq!(attempt.report.writes, 4);
        assert_eq!(attempt.report.writes, d.writes().len());
        assert_eq!(attempt.report.stream_events.len(), 2);
        assert_eq!(d.byte(0x1151), 0x10);
    }

    #[test]
    fn stream_events_do_not_stop_the_remaining_settings() {
        let d = dev();
        let _mic = d.open_mic();
        d.flip_on_newc(0x10);
        let cfg = Config {
            mic_nr: Some(false),
            mic_agc: Some(true),
            sidetone_level: Some(0x0200),
            ..Config::default()
        };
        let report = apply_all(&d, &cfg);
        // two settings with a reload request each, and the level
        assert_eq!(report.writes, 5);
        assert_eq!(report.writes, d.writes().len());
        assert_eq!(report.stream_events.len(), 2);
        assert_eq!(d.byte(0x1151), 0x10);
        assert_eq!(levels(&d), vec![0x0200]);
    }

    #[test]
    fn the_surround_reaches_a_running_playback_stream_by_a_reload_request() {
        let d = dev();
        let _playback = d.open_active_stream().unwrap();
        assert!(!d.surround_applied());

        let applied = set_surround(&mut d.clone(), true).unwrap();
        assert_eq!(applied.writes, 2);
        assert_eq!(applied.stream_event, None);
        assert_eq!(d.writes(), vec![(0x1152, vec![0x08]), (0x1154, vec![0x11])]);
        assert_eq!(d.newc_consumed(), 1);
        assert!(d.surround_applied());

        // it holds: nothing is written and no reload is requested
        d.clear_writes();
        assert!(!put(&d, |t| set_surround(t, true)));
        assert!(d.writes().is_empty());

        assert!(put(&d, |t| set_surround(t, false)));
        assert_eq!(d.byte(0x1152), 0x00);
        assert_eq!(d.newc_consumed(), 2);
        assert!(!d.surround_applied());
    }

    #[test]
    fn the_surround_set_without_a_playback_stream_is_picked_up_by_the_next_one() {
        let d = dev();
        assert!(put(&d, |t| set_surround(t, true)));
        assert_eq!(d.writes(), vec![(0x1152, vec![0x08])]);
        assert_eq!(d.newc_consumed(), 0);
        assert!(!d.surround_applied());
        let _playback = d.open_active_stream().unwrap();
        assert!(d.surround_applied());

        // a capture stream alone is no reason for a reload
        let d = dev();
        let _mic = d.open_mic();
        assert!(put(&d, |t| set_surround(t, true)));
        assert_eq!(d.writes(), vec![(0x1152, vec![0x08])]);
        assert_eq!(d.newc_consumed(), 0);
    }

    #[test]
    fn the_surround_level_is_reloaded_only_while_the_surround_is_on_and_playing() {
        let level = |d: &FakeDevice, level| set_surround_level(&mut d.clone(), level).unwrap();

        // surround off, playing: the byte is written and the ROM does not use it yet
        let d = dev();
        let _playback = d.open_active_stream().unwrap();
        assert_eq!(level(&d, 7).writes, 1);
        assert_eq!(d.writes(), vec![(0x10D3, vec![0x07])]);
        assert_eq!(d.newc_consumed(), 0);

        // surround on, not playing
        let d = dev();
        d.poke(0x1152, &[0x08]);
        assert_eq!(level(&d, 7).writes, 1);
        assert_eq!(d.newc_consumed(), 0);

        // surround on and playing: the running stream gets the new level
        let d = dev();
        d.poke(0x1152, &[0x08]);
        let _playback = d.open_active_stream().unwrap();
        assert_eq!(level(&d, 7).writes, 2);
        assert_eq!(d.writes(), vec![(0x10D3, vec![0x07]), (0x1154, vec![0x11])]);
        assert_eq!(d.newc_consumed(), 1);

        // the level that is there already writes nothing
        d.clear_writes();
        assert_eq!(level(&d, 7).writes, 0);
        assert!(d.writes().is_empty());
    }

    #[test]
    fn a_surround_level_outside_one_to_ten_writes_nothing() {
        for bad in [0, 11, 16, 255] {
            let d = dev();
            assert!(matches!(
                set_surround_level(&mut d.clone(), bad),
                Err(Error::Refused(_))
            ));
            assert!(d.writes().is_empty(), "{bad}");
        }
    }

    #[test]
    fn the_saved_surround_is_applied_before_sidetone_with_its_level() {
        let cfg = Config {
            surround: Some(true),
            surround_level: Some(8),
            sidetone: Some(true),
            sidetone_level: Some(0x0200),
            ..Config::default()
        };
        let d = dev();
        assert_eq!(apply_all(&d, &cfg).writes, 4);
        assert_eq!(
            d.writes(),
            vec![
                (0x1152, vec![0x08]),
                (0x10D3, vec![0x08]),
                (0x1276, vec![0x00, 0x02]),
                (0x1151, vec![0x84]),
            ]
        );
        d.clear_writes();
        assert_eq!(apply_all(&d, &cfg).writes, 0);

        // with a playback stream open each of the two is followed by a reload request
        let d = dev();
        let _playback = d.open_active_stream().unwrap();
        let cfg = Config {
            surround: Some(true),
            surround_level: Some(8),
            ..Config::default()
        };
        assert_eq!(apply_all(&d, &cfg).writes, 4);
        assert_eq!(
            d.writes(),
            vec![
                (0x1152, vec![0x08]),
                (0x1154, vec![0x11]),
                (0x10D3, vec![0x08]),
                (0x1154, vec![0x11]),
            ]
        );
        assert!(d.surround_applied());

        // a switch alone leaves the level as the device has it
        let d = dev();
        let cfg = Config {
            surround: Some(true),
            ..Config::default()
        };
        assert_eq!(apply_all(&d, &cfg).writes, 1);
        assert_eq!(d.byte(0x10D3), 0x04);
    }

    #[test]
    fn a_surround_byte_with_bits_that_are_not_ours_is_left_alone_after_the_earlier_settings() {
        let d = dev();
        d.poke(0x1152, &[0x04]);
        let cfg = Config {
            mic_nr: Some(false),
            surround: Some(true),
            surround_level: Some(8),
            ..Config::default()
        };
        let attempt = apply_config(&mut d.clone(), &cfg);
        assert!(matches!(attempt.error, Some(Error::Refused(_))));
        assert_eq!(attempt.report.writes, 1);
        assert_eq!(d.writes(), vec![(0x1151, vec![0x00])]);
        assert_eq!(d.byte(0x1152), 0x04);
    }

    #[test]
    fn nothing_is_written_when_the_dsp_flags_forbid_it() {
        let d = dev();
        d.poke(0x00A6, &[0x01]);
        assert!(matches!(
            set_noise_reduction(&mut d.clone(), false),
            Err(Error::Refused(_))
        ));
        assert!(matches!(
            set_agc(&mut d.clone(), true),
            Err(Error::Refused(_))
        ));
        assert!(matches!(
            set_sidetone(&mut d.clone(), true),
            Err(Error::Refused(_))
        ));
        assert!(matches!(
            set_sidetone_level(&mut d.clone(), 0x0200),
            Err(Error::Refused(_))
        ));
        assert!(matches!(
            set_surround(&mut d.clone(), true),
            Err(Error::Refused(_))
        ));
        assert!(matches!(
            set_surround_level(&mut d.clone(), 7),
            Err(Error::Refused(_))
        ));
        assert!(d.writes().is_empty());
    }
}
