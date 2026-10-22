//! A playback stream of digital silence on the dongle's output.
//!
//! While a render stream is running, the codec firmware keeps its playback path and headphone
//! driver fully on. Changing the hook or the routine it runs with such a stream open means the
//! driver does not change state during the moment the hook is off.
//!
//! The stream is opened in WASAPI exclusive mode when no other program has an audio session on
//! the output: closing it ends the USB stream at once, so the codec returns to idle. Taking the
//! output exclusively would stop other programs' sessions for good, so when one has a session,
//! or exclusive mode is not available, the stream is opened in shared mode instead. A program
//! that opens a session after the session check but before the exclusive `Initialize` returns
//! is still cut off. After a shared-mode stream closes, Windows often keeps the USB stream
//! running for a long time (see docs/firmware-notes.md).

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED,
    AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioClient, IAudioRenderClient,
    IAudioSessionControl2, IAudioSessionManager2, IDeviceTopology, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::core::Interface;

use crate::mode;
use crate::protocol::{PID, VID};
use crate::{Error, Result};

/// Shared-mode buffer length in 100 ns units.
const BUFFER_100NS: i64 = 2_000_000;
/// How often the buffer is topped up with silence.
const REFILL: Duration = Duration::from_millis(20);
/// Sample rates tried for the exclusive stream, as 16-bit stereo PCM.
const EXCLUSIVE_RATES: [u32; 3] = [48_000, 44_100, 96_000];
/// The exclusive buffer holds this many device periods, so a late refill does not underrun.
const EXCLUSIVE_PERIODS: i64 = 4;

/// The open stream. Dropping it stops the stream.
pub struct SilentStream {
    exclusive: bool,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
    /// Why the stream stopped by itself, if it did.
    failure: Arc<Mutex<Option<String>>>,
}

impl Drop for SilentStream {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl SilentStream {
    /// True if the stream was opened in exclusive mode, false for shared mode.
    pub fn is_exclusive(&self) -> bool {
        self.exclusive
    }
}

impl mode::Stream for SilentStream {
    fn check(&self) -> Result<()> {
        let failure = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        match failure.as_deref() {
            Some(why) => Err(Error::Device(format!("the silent stream stopped: {why}"))),
            None => Ok(()),
        }
    }
}

/// Starts a stream of digital silence on the dongle's playback endpoint and returns once
/// Windows is running it.
pub fn open_silence() -> Result<SilentStream> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let failure = Arc::new(Mutex::new(None));
    let thread = {
        let failure = Arc::clone(&failure);
        thread::spawn(move || render(&started_tx, &stop_rx, &failure))
    };
    let mut stream = SilentStream {
        exclusive: false,
        stop: Some(stop_tx),
        thread: Some(thread),
        failure,
    };
    match started_rx.recv() {
        Ok(Ok(exclusive)) => {
            stream.exclusive = exclusive;
            Ok(stream)
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::Device("the audio thread ended unexpectedly".into())),
    }
}

fn audio_err(e: windows::core::Error) -> Error {
    Error::Device(format!("audio: {e}"))
}

fn render(
    started: &SyncSender<Result<bool>>,
    stop: &Receiver<()>,
    failure: &Mutex<Option<String>>,
) {
    // SAFETY: COM is initialised on this thread before any COM call and uninitialised after
    // every COM object created by `run` has been dropped.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if let Err(e) = init.ok() {
        let _ = started.send(Err(audio_err(e)));
        return;
    }
    if let Err(e) = run(started, stop, failure) {
        let _ = started.send(Err(e));
    }
    unsafe { CoUninitialize() };
}

/// An initialised audio client with its render service.
struct Output {
    client: IAudioClient,
    render: IAudioRenderClient,
    frames: u32,
    /// Bytes per frame.
    block: usize,
    exclusive: bool,
}

fn run(
    started: &SyncSender<Result<bool>>,
    stop: &Receiver<()>,
    failure: &Mutex<Option<String>>,
) -> Result<()> {
    // SAFETY: plain WASAPI calls on objects created here; each buffer returned by GetBuffer
    // is zeroed and released before the next call.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(audio_err)?;
        let device = find_dongle(&enumerator)?.ok_or_else(|| {
            Error::Device(format!(
                "no playback endpoint of the dongle ({VID:04X}:{PID:04X}) found"
            ))
        })?;
        let out = open_output(&device)?;
        silence(&out, out.frames)?;
        out.client.Start().map_err(audio_err)?;
        let _ = started.send(Ok(out.exclusive));
        let ended = refill(&out, stop);
        if let Err(e) = ended {
            *failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
        }
        let _ = out.client.Stop();
        Ok(())
    }
}

/// Keeps the buffer full of silence until the stream is dropped.
unsafe fn refill(out: &Output, stop: &Receiver<()>) -> Result<()> {
    unsafe {
        while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(REFILL) {
            let free = out.frames - out.client.GetCurrentPadding().map_err(audio_err)?;
            if free > 0 {
                silence(out, free)?;
            }
        }
        Ok(())
    }
}

unsafe fn open_output(device: &IMMDevice) -> Result<Output> {
    unsafe {
        if let Some(out) = open_exclusive(device) {
            return Ok(out);
        }
        open_shared(device)
    }
}

fn pcm16_stereo(rate: u32) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 2,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * 4,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// An exclusive-mode stream, or `None` if the endpoint does not offer one or another program
/// has an audio session on it. The format and period are probed first so that the session
/// check runs immediately before `Initialize`.
unsafe fn open_exclusive(device: &IMMDevice) -> Option<Output> {
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
        let format = EXCLUSIVE_RATES.into_iter().map(pcm16_stereo).find(|f| {
            client
                .IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, f, None)
                .0
                == 0
        })?;
        let mut period = 0;
        client.GetDevicePeriod(Some(&mut period), None).ok()?;
        if has_other_sessions(device).unwrap_or(true) {
            return None;
        }
        client
            .Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                0,
                period * EXCLUSIVE_PERIODS,
                period,
                &format,
                None,
            )
            .ok()?;
        output(client, format.nBlockAlign, true).ok()
    }
}

unsafe fn open_shared(device: &IMMDevice) -> Result<Output> {
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(audio_err)?;
        let format = client.GetMixFormat().map_err(audio_err)?;
        let block = format.read_unaligned().nBlockAlign;
        let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_100NS, 0, format, None);
        CoTaskMemFree(Some(format as *const _));
        init.map_err(audio_err)?;
        output(client, block, false)
    }
}

unsafe fn output(client: IAudioClient, block: u16, exclusive: bool) -> Result<Output> {
    unsafe {
        Ok(Output {
            render: client.GetService().map_err(audio_err)?,
            frames: client.GetBufferSize().map_err(audio_err)?,
            block: block as usize,
            exclusive,
            client,
        })
    }
}

/// Writes `frames` frames of zeros, flagged as silence.
unsafe fn silence(out: &Output, frames: u32) -> Result<()> {
    unsafe {
        let buffer = out.render.GetBuffer(frames).map_err(audio_err)?;
        std::ptr::write_bytes(buffer, 0, frames as usize * out.block);
        out.render
            .ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
            .map_err(audio_err)
    }
}

/// True if a program other than the system sounds and this process has an audio session on the
/// endpoint, playing or not.
unsafe fn has_other_sessions(device: &IMMDevice) -> windows::core::Result<bool> {
    unsafe {
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let sessions = manager.GetSessionEnumerator()?;
        for i in 0..sessions.GetCount()? {
            let session: IAudioSessionControl2 = sessions.GetSession(i)?.cast()?;
            let system = session.IsSystemSoundsSession().0 == 0;
            let expired = session.GetState()? == AudioSessionStateExpired;
            let own = session.GetProcessId()? == std::process::id();
            if !system && !expired && !own {
                return Ok(true);
            }
        }
        Ok(false)
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

    #[test]
    fn exclusive_formats_are_16_bit_stereo_pcm() {
        for rate in EXCLUSIVE_RATES {
            let f = pcm16_stereo(rate);
            let (tag, channels, per_sec, avg, block, bits) = (
                f.wFormatTag,
                f.nChannels,
                f.nSamplesPerSec,
                f.nAvgBytesPerSec,
                f.nBlockAlign,
                f.wBitsPerSample,
            );
            assert_eq!((tag, channels, bits, block), (1, 2, 16, 4));
            assert_eq!((per_sec, avg), (rate, rate * 4));
        }
    }
}
