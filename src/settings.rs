//! Applying the saved settings to the device.

use crate::config::Config;
use crate::dsp::{self, Attempt};
use crate::mode::{self, Mode, Stream};
use crate::protocol::Transport;
use crate::{Error, Result};

/// Applies a mode, then the min-hiss settle time if one is given and the mode is min-hiss.
/// Returns the number of writes made.
pub fn set_mode<T: Transport + ?Sized, S: Stream>(
    dev: &mut T,
    target: Mode,
    min_hiss_ms: Option<u16>,
    open_stream: impl FnOnce() -> Result<S>,
) -> Result<usize> {
    let mut n = mode::apply_mode(dev, target, open_stream)?;
    if target == Mode::MinHiss
        && let Some(ms) = min_hiss_ms
    {
        n += usize::from(mode::tune_min_hiss(dev, ms)?);
    }
    Ok(n)
}

/// Applies the saved settings. Idempotent: writes nothing if the device already matches.
///
/// The microphone DSP settings come first. A DSP reload request is awaited before the mode
/// change opens its own playback stream, so the request and the stream's open and close do not
/// overlap, and the ROM's reload at that stream's start sees the settings already in place.
/// A DSP setting that is refused writes nothing, so the mode and the mic boost are still applied
/// and the refusal is in the result afterwards. The report always holds what was done before the
/// error; the writes of a mode change that fails part-way are not counted.
pub fn ensure<T: Transport + ?Sized, S: Stream>(
    dev: &mut T,
    cfg: &Config,
    open_stream: impl FnOnce() -> Result<S>,
) -> Attempt {
    let Attempt {
        mut report,
        error: dsp_error,
    } = dsp::apply_config(dev, cfg);
    if dsp_error
        .as_ref()
        .is_some_and(|e| !matches!(e, Error::Refused(_)))
    {
        return Attempt {
            report,
            error: dsp_error,
        };
    }
    if let Some(target) = cfg.mode {
        match set_mode(dev, target, cfg.min_hiss_ms, open_stream) {
            Ok(writes) => report.writes += writes,
            Err(error) => {
                return Attempt {
                    report,
                    error: Some(error),
                };
            }
        }
    }
    if let Some(step) = cfg.mic_boost {
        match mode::set_mic_boost(dev, step) {
            Ok(written) => report.writes += usize::from(written),
            Err(error) => {
                return Attempt {
                    report,
                    error: Some(error),
                };
            }
        }
    }
    Attempt {
        report,
        error: dsp_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::Report;
    use crate::fake::FakeDevice;
    use crate::patches::StreamEvent;

    fn config() -> Config {
        Config {
            mode: Some(Mode::Balanced),
            mic_boost: Some(6),
            mic_nr: Some(false),
            mic_agc: Some(true),
            sidetone_level: Some(0x0200),
            ..Config::default()
        }
    }

    fn attempt(dev: &FakeDevice, cfg: &Config) -> Attempt {
        ensure(&mut dev.clone(), cfg, || dev.open_active_stream())
    }

    /// Applies `cfg` and asserts that nothing went wrong.
    fn run(dev: &FakeDevice, cfg: &Config) -> Report {
        let attempt = attempt(dev, cfg);
        assert!(attempt.error.is_none(), "{:?}", attempt.error);
        attempt.report
    }

    fn is_mode_write(addr: u16) -> bool {
        [
            0x14B1..=0x14B4,
            0x16CF..=0x16CF,
            0x17D0..=0x17D0,
            0x1EFC..=0x1F26,
        ]
        .iter()
        .any(|r| r.contains(&addr))
    }

    #[test]
    fn the_dsp_settings_are_applied_before_the_mode_change() {
        let dev = FakeDevice::stock();
        let _mic = dev.open_mic();
        let report = run(&dev, &config());

        let writes = dev.writes();
        assert_eq!(report.writes, writes.len());
        assert!(report.stream_events.is_empty());
        let last_dsp = writes
            .iter()
            .rposition(|(addr, _)| (0x1151..=0x1154).contains(addr) || *addr == 0x1276)
            .unwrap();
        let first_mode = writes
            .iter()
            .position(|(addr, _)| is_mode_write(*addr))
            .unwrap();
        assert!(last_dsp < first_mode, "{writes:02x?}");
        assert!(writes.iter().any(|(addr, _)| *addr == 0x1154));

        let state = mode::read_state(&mut dev.clone()).unwrap();
        assert_eq!(state.mode(), Some(Mode::Balanced));
        assert_eq!(state.mic_boost(), Some(6));
        assert_eq!(state.dsp_enable, 0x10);
        assert_eq!(state.sidetone_level, 0x0200);
    }

    #[test]
    fn a_stream_event_near_the_reload_does_not_stop_the_other_settings() {
        let dev = FakeDevice::stock();
        let _mic = dev.open_mic();
        dev.flip_on_newc(0x10);
        let report = run(&dev, &config());

        assert_eq!(report.stream_events.len(), 2);
        assert!(report.stream_events.contains(&StreamEvent {
            before: 0x02,
            after: 0x12
        }));
        let state = mode::read_state(&mut dev.clone()).unwrap();
        assert_eq!(state.mode(), Some(Mode::Balanced));
        assert_eq!(state.mic_boost(), Some(6));
        assert_eq!(state.dsp_enable, 0x10);
    }

    #[test]
    fn a_reload_the_rom_has_pending_does_not_stop_the_other_settings() {
        let dev = FakeDevice::stock();
        let _mic = dev.open_mic();
        dev.raise_newc(9);
        let report = run(&dev, &config());

        assert!(report.stream_events.is_empty());
        let state = mode::read_state(&mut dev.clone()).unwrap();
        assert_eq!(state.mode(), Some(Mode::Balanced));
        assert_eq!(state.dsp_enable, 0x10);
        assert_eq!(dev.byte(0x1154) & 0x01, 0);
    }

    #[test]
    fn nothing_is_written_when_the_device_already_matches() {
        let dev = FakeDevice::stock();
        run(&dev, &config());
        dev.clear_writes();
        let report = run(&dev, &config());
        assert_eq!(report, Report::default());
        assert!(dev.writes().is_empty());
    }

    #[test]
    fn a_refused_dsp_setting_does_not_keep_the_mode_from_being_applied() {
        let dev = FakeDevice::stock();
        // the DSP bypass flag refuses every DSP write and does not concern the mode
        dev.poke(0x00A6, &[0x01]);
        let attempt = attempt(&dev, &config());
        assert!(
            matches!(attempt.error, Some(Error::Refused(_))),
            "{:?}",
            attempt.error
        );
        assert!(
            dev.writes()
                .iter()
                .all(|(addr, _)| ![0x1151, 0x1154, 0x1276].contains(addr))
        );
        assert_eq!(attempt.report.writes, dev.writes().len());
        assert!(attempt.report.writes > 0);
        let state = mode::read_state(&mut dev.clone()).unwrap();
        assert_eq!(state.mode(), Some(Mode::Balanced));
        assert_eq!(state.mic_boost(), Some(6));
    }

    #[test]
    fn a_refused_dsp_setting_keeps_the_writes_and_warnings_made_before_it() {
        let dev = FakeDevice::stock();
        let _mic = dev.open_mic();
        dev.flip_on_newc(0x10);
        // noise reduction and AGC are applied, then the sidetone level is refused
        dev.poke(0x1276, &0x8000u16.to_le_bytes());
        let attempt = attempt(&dev, &config());

        assert!(matches!(attempt.error, Some(Error::Refused(_))));
        assert_eq!(attempt.report.stream_events.len(), 2);
        // four DSP writes and the mode and mic boost writes that followed the refusal
        assert_eq!(attempt.report.writes, dev.writes().len());
        assert!(attempt.report.writes > 4);
        let state = mode::read_state(&mut dev.clone()).unwrap();
        assert_eq!(state.mode(), Some(Mode::Balanced));
        assert_eq!(state.mic_boost(), Some(6));
        assert_eq!(state.dsp_enable, 0x10);
    }

    #[test]
    fn a_device_error_in_the_dsp_settings_stops_everything() {
        let dev = FakeDevice::stock();
        dev.fail_after_writes(0);
        let attempt = attempt(&dev, &config());
        assert!(
            matches!(attempt.error, Some(Error::Device(_))),
            "{:?}",
            attempt.error
        );
        assert_eq!(attempt.report, Report::default());
        assert!(dev.writes().is_empty());
        assert_eq!(dev.streams_opened(), 0);
    }

    #[test]
    fn a_device_error_after_some_writes_keeps_the_report_of_those() {
        let dev = FakeDevice::stock();
        // noise reduction (1 write) goes through; AGC is the second write and fails
        dev.fail_after_writes(1);
        let attempt = attempt(&dev, &config());
        assert!(matches!(attempt.error, Some(Error::Device(_))));
        assert_eq!(attempt.report.writes, 1);
        assert_eq!(dev.writes().len(), 1);
        assert_eq!(dev.streams_opened(), 0);
    }

    #[test]
    fn a_mode_error_keeps_the_report_of_the_dsp_settings() {
        let dev = FakeDevice::stock();
        dev.remove_endpoint();
        let attempt = attempt(&dev, &config());
        assert!(matches!(attempt.error, Some(Error::Audio(_))));
        // noise reduction, AGC and the sidetone level
        assert_eq!(attempt.report.writes, 3);
        assert_eq!(dev.writes().len(), 3);
    }

    #[test]
    fn a_mic_boost_error_keeps_the_report_of_the_mode_change() {
        let dev = FakeDevice::in_mode(Mode::Quiet);
        let cfg = Config {
            mode: Some(Mode::Stock),
            mic_boost: Some(6),
            ..Config::default()
        };
        // the mode change makes three writes (the hook and the two DAC holds); the mic boost is
        // the fourth
        dev.fail_after_writes(3);
        let attempt = attempt(&dev, &cfg);
        assert!(matches!(attempt.error, Some(Error::Device(_))));
        assert_eq!(attempt.report.writes, 3);
        assert_eq!(dev.byte(0x1017), 0x3C);
    }
}
