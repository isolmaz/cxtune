//! Applying the saved settings to the device.

use crate::config::Config;
use crate::dsp::{self, Report};
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
/// and the refusal is returned afterwards.
pub fn ensure<T: Transport + ?Sized, S: Stream>(
    dev: &mut T,
    cfg: &Config,
    open_stream: impl FnOnce() -> Result<S>,
) -> Result<Report> {
    let dsp = dsp::apply_config(dev, cfg);
    if matches!(&dsp, Err(e) if !matches!(e, Error::Refused(_))) {
        return dsp;
    }
    let mut writes = 0;
    if let Some(target) = cfg.mode {
        writes += set_mode(dev, target, cfg.min_hiss_ms, open_stream)?;
    }
    if let Some(step) = cfg.mic_boost {
        writes += usize::from(mode::set_mic_boost(dev, step)?);
    }
    dsp.map(|mut report| {
        report.writes += writes;
        report
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn run(dev: &FakeDevice, cfg: &Config) -> Result<Report> {
        ensure(&mut dev.clone(), cfg, || dev.open_active_stream())
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
        let report = run(&dev, &config()).unwrap();

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
        let report = run(&dev, &config()).unwrap();

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
        let report = run(&dev, &config()).unwrap();

        assert!(report.stream_events.is_empty());
        let state = mode::read_state(&mut dev.clone()).unwrap();
        assert_eq!(state.mode(), Some(Mode::Balanced));
        assert_eq!(state.dsp_enable, 0x10);
        assert_eq!(dev.byte(0x1154) & 0x01, 0);
    }

    #[test]
    fn nothing_is_written_when_the_device_already_matches() {
        let dev = FakeDevice::stock();
        run(&dev, &config()).unwrap();
        dev.clear_writes();
        let report = run(&dev, &config()).unwrap();
        assert_eq!(report, Report::default());
        assert!(dev.writes().is_empty());
    }

    #[test]
    fn a_refused_dsp_setting_does_not_keep_the_mode_from_being_applied() {
        let dev = FakeDevice::stock();
        // the DSP bypass flag refuses every DSP write and does not concern the mode
        dev.poke(0x00A6, &[0x01]);
        let err = run(&dev, &config()).unwrap_err();
        assert!(matches!(err, Error::Refused(_)), "{err}");
        assert!(
            dev.writes()
                .iter()
                .all(|(addr, _)| ![0x1151, 0x1154, 0x1276].contains(addr))
        );
        let state = mode::read_state(&mut dev.clone()).unwrap();
        assert_eq!(state.mode(), Some(Mode::Balanced));
        assert_eq!(state.mic_boost(), Some(6));
    }

    #[test]
    fn a_device_error_in_the_dsp_settings_stops_everything() {
        let dev = FakeDevice::stock();
        dev.fail_after_writes(0);
        let err = run(&dev, &config()).unwrap_err();
        assert!(matches!(err, Error::Device(_)), "{err}");
        assert!(dev.writes().is_empty());
        assert_eq!(dev.streams_opened(), 0);
    }
}
