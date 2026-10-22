//! A playback stream of digital silence on the dongle's output (WASAPI shared mode).
//!
//! While a render stream is running, the codec firmware keeps its playback path and headphone
//! driver fully on. Changing the hook or the routine it runs with such a stream open means the
//! driver does not change state during the moment the hook is off.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE, IAudioClient,
    IAudioRenderClient, IDeviceTopology, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};

use crate::protocol::{PID, VID};
use crate::{Error, Result};

/// Shared-mode buffer length in 100 ns units.
const BUFFER_100NS: i64 = 2_000_000;
/// How often the buffer is topped up with silence.
const REFILL: Duration = Duration::from_millis(50);

/// The open stream. Dropping it stops the stream.
pub struct SilentStream {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for SilentStream {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts a stream of digital silence on the dongle's playback endpoint and returns once
/// Windows is running it.
pub fn open_silence() -> Result<SilentStream> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let thread = thread::spawn(move || render(&started_tx, &stop_rx));
    let stream = SilentStream {
        stop: Some(stop_tx),
        thread: Some(thread),
    };
    match started_rx.recv() {
        Ok(Ok(())) => Ok(stream),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::Device("the audio thread ended unexpectedly".into())),
    }
}

fn audio_err(e: windows::core::Error) -> Error {
    Error::Device(format!("audio: {e}"))
}

fn render(started: &SyncSender<Result<()>>, stop: &Receiver<()>) {
    // SAFETY: COM is initialised on this thread before any COM call and uninitialised after
    // every COM object created by `run` has been dropped.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if let Err(e) = init.ok() {
        let _ = started.send(Err(audio_err(e)));
        return;
    }
    if let Err(e) = run(started, stop) {
        let _ = started.send(Err(e));
    }
    unsafe { CoUninitialize() };
}

fn run(started: &SyncSender<Result<()>>, stop: &Receiver<()>) -> Result<()> {
    // SAFETY: plain WASAPI calls on objects created here; buffers returned by GetBuffer are
    // released with ReleaseBuffer before the next call and never read or written.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(audio_err)?;
        let device = find_dongle(&enumerator)?.ok_or_else(|| {
            Error::Device(format!(
                "no playback endpoint of the dongle ({VID:04X}:{PID:04X}) found"
            ))
        })?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(audio_err)?;
        let format = client.GetMixFormat().map_err(audio_err)?;
        let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_100NS, 0, format, None);
        CoTaskMemFree(Some(format as *const _));
        init.map_err(audio_err)?;
        let render: IAudioRenderClient = client.GetService().map_err(audio_err)?;
        let frames = client.GetBufferSize().map_err(audio_err)?;
        silence(&render, frames)?;
        client.Start().map_err(audio_err)?;
        let _ = started.send(Ok(()));
        while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(REFILL) {
            let free = frames - client.GetCurrentPadding().map_err(audio_err)?;
            if free > 0 {
                silence(&render, free)?;
            }
        }
        client.Stop().map_err(audio_err)
    }
}

unsafe fn silence(render: &IAudioRenderClient, frames: u32) -> Result<()> {
    unsafe {
        render.GetBuffer(frames).map_err(audio_err)?;
        render
            .ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
            .map_err(audio_err)
    }
}

unsafe fn find_dongle(enumerator: &IMMDeviceEnumerator) -> Result<Option<IMMDevice>> {
    unsafe {
        let devices = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(audio_err)?;
        for i in 0..devices.GetCount().map_err(audio_err)? {
            let device = devices.Item(i).map_err(audio_err)?;
            if is_dongle(&device).unwrap_or(false) {
                return Ok(Some(device));
            }
        }
        Ok(None)
    }
}

/// True if the endpoint belongs to the dongle: the USB device behind the endpoint carries
/// its vendor and product id.
unsafe fn is_dongle(device: &IMMDevice) -> windows::core::Result<bool> {
    unsafe {
        let topology: IDeviceTopology = device.Activate(CLSCTX_ALL, None)?;
        let id = topology.GetConnector(0)?.GetDeviceIdConnectedTo()?;
        let text = id.to_string();
        CoTaskMemFree(Some(id.as_ptr() as *const _));
        Ok(text.is_ok_and(|s| is_dongle_id(&s)))
    }
}

fn is_dongle_id(device_id: &str) -> bool {
    device_id
        .to_ascii_lowercase()
        .contains(&format!("vid_{VID:04x}&pid_{PID:04x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_usb_interface_path_of_the_dongle_only() {
        assert!(is_dongle_id(
            r"{2}.\\?\usb#vid_0951&pid_16a4&mi_00#7&3321d09b&0&0000#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\global"
        ));
        assert!(is_dongle_id(r"{2}.\\?\USB#VID_0951&PID_16A4&MI_00#x"));
        assert!(!is_dongle_id(r"{2}.\\?\usb#vid_0951&pid_16a5&mi_00#x"));
        assert!(!is_dongle_id(r"{2}.\\?\usb#vid_0952&pid_16a4&mi_00#x"));
        assert!(!is_dongle_id(r"{2}.\\?\hdaudio#func_01&ven_10ec#x"));
    }
}
