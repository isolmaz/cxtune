//! The volume and the mute switch of the dongle's audio endpoints, through the Windows endpoint
//! volume interface. This is the volume of the Windows mixer and of the buttons on the dongle;
//! no HID report is sent.
//!
//! The endpoints are found like the playback endpoint of [`crate::audio`]: the USB device
//! behind the endpoint carries the dongle's vendor and product id.

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::core::GUID;

use crate::audio::is_dongle;
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The headphones.
    Playback,
    /// The microphone.
    Capture,
}

/// The volume as a whole percent, and the mute switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    pub percent: u8,
    pub muted: bool,
}

fn percent_of(scalar: f32) -> u8 {
    (scalar.clamp(0.0, 1.0) * 100.0).round() as u8
}

fn scalar_of(percent: u8) -> f32 {
    f32::from(percent.min(100)) / 100.0
}

fn audio_err(e: windows::core::Error) -> Error {
    Error::Audio(e.to_string())
}

/// Ends the COM use of the thread when it is dropped; declared after the interfaces so that
/// they are released first.
struct Com;

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: paired with the successful `CoInitializeEx` of `Volume::open` on this thread.
        unsafe { CoUninitialize() };
    }
}

/// The two endpoints, on the thread that opened them (COM objects stay on their thread).
pub struct Volume {
    playback: Option<IAudioEndpointVolume>,
    capture: Option<IAudioEndpointVolume>,
    _com: Com,
}

impl Volume {
    /// Starts COM on the calling thread and looks for the endpoints. An endpoint that is not
    /// there yet (the dongle is not plugged in) is looked for again by [`Volume::refresh`].
    pub fn open() -> Result<Volume> {
        // SAFETY: a plain COM initialisation of this thread, undone by `Com`.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(audio_err)?;
        let mut volume = Volume {
            playback: None,
            capture: None,
            _com: Com,
        };
        volume.refresh();
        Ok(volume)
    }

    /// Looks for the endpoints that are missing.
    pub fn refresh(&mut self) {
        if self.playback.is_some() && self.capture.is_some() {
            return;
        }
        // SAFETY: COM is initialised on this thread.
        let Ok(enumerator) = (unsafe {
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        }) else {
            return;
        };
        if self.playback.is_none() {
            self.playback = find(&enumerator, eRender);
        }
        if self.capture.is_none() {
            self.capture = find(&enumerator, eCapture);
        }
    }

    fn endpoint(&mut self, side: Side) -> &mut Option<IAudioEndpointVolume> {
        match side {
            Side::Playback => &mut self.playback,
            Side::Capture => &mut self.capture,
        }
    }

    /// The level of the endpoint, or `None` if it is not there. An endpoint that fails to answer
    /// (the dongle was unplugged) is forgotten.
    pub fn level(&mut self, side: Side) -> Option<Level> {
        let endpoint = self.endpoint(side);
        let volume = endpoint.as_ref()?;
        // SAFETY: plain calls on a COM interface of this thread.
        let read = unsafe {
            volume
                .GetMasterVolumeLevelScalar()
                .and_then(|scalar| volume.GetMute().map(|muted| (scalar, muted.as_bool())))
        };
        match read {
            Ok((scalar, muted)) => Some(Level {
                percent: percent_of(scalar),
                muted,
            }),
            Err(_) => {
                *endpoint = None;
                None
            }
        }
    }

    pub fn set_percent(&mut self, side: Side, percent: u8) -> Result<()> {
        self.write(side, |volume| {
            // SAFETY: plain call on a COM interface of this thread; no event context.
            unsafe { volume.SetMasterVolumeLevelScalar(scalar_of(percent), &GUID::zeroed()) }
        })
    }

    pub fn set_muted(&mut self, side: Side, muted: bool) -> Result<()> {
        self.write(side, |volume| {
            // SAFETY: as above.
            unsafe { volume.SetMute(muted, &GUID::zeroed()) }
        })
    }

    fn write(
        &mut self,
        side: Side,
        call: impl FnOnce(&IAudioEndpointVolume) -> windows::core::Result<()>,
    ) -> Result<()> {
        let endpoint = self.endpoint(side);
        let Some(volume) = endpoint.as_ref() else {
            return Err(Error::Audio("the dongle has no such audio endpoint".into()));
        };
        call(volume).map_err(|e| {
            *endpoint = None;
            audio_err(e)
        })
    }
}

fn find(enumerator: &IMMDeviceEnumerator, flow: EDataFlow) -> Option<IAudioEndpointVolume> {
    // SAFETY: plain calls on COM objects of this thread.
    unsafe {
        let devices = enumerator
            .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
            .ok()?;
        (0..devices.GetCount().ok()?).find_map(|i| {
            let device = devices.Item(i).ok()?;
            if !is_dongle(&device).unwrap_or(false) {
                return None;
            }
            device
                .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
                .ok()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scalar_becomes_the_nearest_whole_percent() {
        assert_eq!(percent_of(0.0), 0);
        assert_eq!(percent_of(1.0), 100);
        assert_eq!(percent_of(0.504), 50);
        assert_eq!(percent_of(0.506), 51);
    }

    #[test]
    fn a_scalar_outside_the_range_is_clamped() {
        assert_eq!(percent_of(-0.2), 0);
        assert_eq!(percent_of(1.7), 100);
    }

    #[test]
    fn every_percent_survives_the_round_trip_through_a_scalar() {
        for percent in 0..=100 {
            assert_eq!(percent_of(scalar_of(percent)), percent);
        }
    }

    #[test]
    fn a_percent_above_100_is_full_volume() {
        assert_eq!(scalar_of(250), 1.0);
    }
}
