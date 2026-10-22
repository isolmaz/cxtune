//! Playback streams on the dongle's output: digital silence for the mode changes, and short
//! mono clips (the test tone and recordings) for the test helpers.
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
//! running for a long time (see docs/firmware-notes.md). A clip is played the same way, in 16-bit
//! stereo at 48 kHz, so that closing its stream returns the codec to idle as well.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AudioSessionStateExpired, DEVICE_STATE_ACTIVE, EDataFlow, IAudioClient, IAudioRenderClient,
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
/// How often the buffer is topped up.
const REFILL: Duration = Duration::from_millis(20);
/// Sample rates tried for the exclusive stream, as 16-bit stereo PCM.
const EXCLUSIVE_RATES: [u32; 3] = [48_000, 44_100, 96_000];
/// The exclusive buffer holds this many device periods, so a late refill does not underrun.
const EXCLUSIVE_PERIODS: i64 = 4;
/// Sample rate of the clips [`play`] plays.
pub const PLAY_RATE: u32 = 48_000;

/// What a render stream plays.
enum Source {
    Silence,
    /// Mono samples at [`PLAY_RATE`], played on both channels.
    Clip(Vec<i16>),
}

/// An open render stream. Dropping it stops the stream.
struct Render {
    exclusive: bool,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
    /// Why the stream stopped by itself, if it did.
    failure: Arc<Mutex<Option<String>>>,
    /// Every sample of a clip has been played out.
    finished: Arc<AtomicBool>,
}

impl Drop for Render {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Render {
    fn check(&self) -> Result<()> {
        let failure = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        match failure.as_deref() {
            Some(why) => Err(Error::Audio(format!("the stream stopped: {why}"))),
            None => Ok(()),
        }
    }
}

/// The open stream of digital silence. Dropping it stops the stream.
pub struct SilentStream(Render);

impl SilentStream {
    /// True if the stream was opened in exclusive mode, false for shared mode.
    pub fn is_exclusive(&self) -> bool {
        self.0.exclusive
    }
}

impl mode::Stream for SilentStream {
    fn check(&self) -> Result<()> {
        self.0
            .check()
            .map_err(|e| Error::Audio(format!("the silent {e}")))
    }
}

/// Starts a stream of digital silence on the dongle's playback endpoint and returns once
/// Windows is running it.
pub fn open_silence() -> Result<SilentStream> {
    start(Source::Silence).map(SilentStream)
}

/// A clip playing on the dongle's output. Dropping it stops the stream, finished or not.
pub struct Playback(Render);

impl Playback {
    /// True if the stream was opened in exclusive mode, false for shared mode.
    pub fn is_exclusive(&self) -> bool {
        self.0.exclusive
    }

    /// True once the last sample has been played out; an error if the stream stopped by itself.
    pub fn is_finished(&self) -> Result<bool> {
        self.0.check()?;
        Ok(self.0.finished.load(Ordering::SeqCst))
    }
}

/// Plays `samples` (mono, [`PLAY_RATE`]) on the dongle's playback endpoint and returns once
/// Windows is running the stream.
pub fn play(samples: Vec<i16>) -> Result<Playback> {
    start(Source::Clip(samples)).map(Playback)
}

fn start(source: Source) -> Result<Render> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let failure = Arc::new(Mutex::new(None));
    let finished = Arc::new(AtomicBool::new(false));
    let thread = {
        let (failure, finished) = (Arc::clone(&failure), Arc::clone(&finished));
        thread::spawn(move || render(source, &started_tx, &stop_rx, &failure, &finished))
    };
    let mut stream = Render {
        exclusive: false,
        stop: Some(stop_tx),
        thread: Some(thread),
        failure,
        finished,
    };
    match started_rx.recv() {
        Ok(Ok(exclusive)) => {
            stream.exclusive = exclusive;
            Ok(stream)
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::Audio("the audio thread ended unexpectedly".into())),
    }
}

fn audio_err(e: windows::core::Error) -> Error {
    Error::Audio(e.to_string())
}

fn render(
    source: Source,
    started: &SyncSender<Result<bool>>,
    stop: &Receiver<()>,
    failure: &Mutex<Option<String>>,
    finished: &AtomicBool,
) {
    // SAFETY: COM is initialised on this thread before any COM call and uninitialised after
    // every COM object created by `run` has been dropped.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if let Err(e) = init.ok() {
        let _ = started.send(Err(audio_err(e)));
        return;
    }
    if let Err(e) = run(source, started, stop, failure, finished) {
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

/// The frames still to be written to a stream.
struct Feed {
    clip: Vec<i16>,
    /// Frames of the clip handed to the device so far.
    cursor: usize,
    /// Frames handed to the device so far, clip and silence.
    queued: u64,
    is_clip: bool,
}

impl Feed {
    fn new(source: Source) -> Feed {
        let (clip, is_clip) = match source {
            Source::Silence => (Vec::new(), false),
            Source::Clip(clip) => (clip, true),
        };
        Feed {
            clip,
            cursor: 0,
            queued: 0,
            is_clip,
        }
    }

    /// Whether the device has played the last frame of the clip, given the frames it still holds.
    fn finished(&self, padding: u32) -> bool {
        self.is_clip
            && self.cursor == self.clip.len()
            && self.queued.saturating_sub(u64::from(padding)) >= self.clip.len() as u64
    }
}

fn run(
    source: Source,
    started: &SyncSender<Result<bool>>,
    stop: &Receiver<()>,
    failure: &Mutex<Option<String>>,
    finished: &AtomicBool,
) -> Result<()> {
    // SAFETY: plain WASAPI calls on objects created here; each buffer returned by GetBuffer
    // is written completely and released before the next call.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(audio_err)?;
        let device = find_dongle(&enumerator, eRender)?.ok_or_else(|| {
            Error::Audio(format!(
                "no playback endpoint of the dongle ({VID:04X}:{PID:04X}) found"
            ))
        })?;
        let mut feed = Feed::new(source);
        let out = open_output(&device, feed.is_clip)?;
        fill(&out, &mut feed, out.frames)?;
        out.client.Start().map_err(audio_err)?;
        let _ = started.send(Ok(out.exclusive));
        let ended = refill(&out, &mut feed, stop, finished);
        if let Err(e) = ended {
            *failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
        }
        let _ = out.client.Stop();
        Ok(())
    }
}

/// Keeps the buffer full until the stream is dropped.
unsafe fn refill(
    out: &Output,
    feed: &mut Feed,
    stop: &Receiver<()>,
    finished: &AtomicBool,
) -> Result<()> {
    unsafe {
        while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(REFILL) {
            let padding = out.client.GetCurrentPadding().map_err(audio_err)?;
            if out.frames > padding {
                fill(out, feed, out.frames - padding)?;
            }
            if feed.finished(padding) {
                finished.store(true, Ordering::SeqCst);
            }
        }
        Ok(())
    }
}

unsafe fn open_output(device: &IMMDevice, clip: bool) -> Result<Output> {
    unsafe {
        let rates: &[u32] = if clip { &[PLAY_RATE] } else { &EXCLUSIVE_RATES };
        if let Some(out) = open_exclusive(device, rates) {
            return Ok(out);
        }
        if clip {
            open_shared_clip(device)
        } else {
            open_shared(device)
        }
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
unsafe fn open_exclusive(device: &IMMDevice, rates: &[u32]) -> Option<Output> {
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
        let format = rates.iter().copied().map(pcm16_stereo).find(|f| {
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

/// A shared-mode stream in the clip format; the audio engine converts to its mix format.
unsafe fn open_shared_clip(device: &IMMDevice) -> Result<Output> {
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(audio_err)?;
        let format = pcm16_stereo(PLAY_RATE);
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_100NS,
                0,
                &format,
                None,
            )
            .map_err(audio_err)?;
        output(client, format.nBlockAlign, false)
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

/// Writes `frames` frames into the buffer of the stream.
unsafe fn fill(out: &Output, feed: &mut Feed, frames: u32) -> Result<()> {
    unsafe {
        let buffer = out.render.GetBuffer(frames).map_err(audio_err)?;
        let bytes = std::slice::from_raw_parts_mut(buffer, frames as usize * out.block);
        let flags = pack(bytes, frames, feed);
        out.render.ReleaseBuffer(frames, flags).map_err(audio_err)
    }
}

/// Fills `bytes`, `frames` frames of the stream's format, with the next samples of the clip on both
/// channels (a clip is only played in 16-bit stereo) and then zeros. Returns the buffer flags: a
/// buffer without a sample of the clip is flagged as silence.
fn pack(bytes: &mut [u8], frames: u32, feed: &mut Feed) -> u32 {
    bytes.fill(0);
    let left = feed.clip.len() - feed.cursor;
    let samples = &feed.clip[feed.cursor..feed.cursor + left.min(frames as usize)];
    for (frame, sample) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(samples) {
        let le = sample.to_le_bytes();
        frame[..2].copy_from_slice(&le);
        frame[2..].copy_from_slice(&le);
    }
    feed.cursor += samples.len();
    feed.queued += u64::from(frames);
    if samples.is_empty() {
        AUDCLNT_BUFFERFLAGS_SILENT.0 as u32
    } else {
        0
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

pub(crate) unsafe fn find_dongle(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
) -> Result<Option<IMMDevice>> {
    unsafe {
        let devices = enumerator
            .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
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
pub(crate) unsafe fn is_dongle(device: &IMMDevice) -> windows::core::Result<bool> {
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

    /// The frames of a buffer as (left, right).
    fn frames_of(bytes: &[u8]) -> Vec<(i16, i16)> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|f| {
                (
                    i16::from_le_bytes([f[0], f[1]]),
                    i16::from_le_bytes([f[2], f[3]]),
                )
            })
            .collect()
    }

    const SILENT: u32 = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;

    #[test]
    fn a_clip_is_written_on_both_channels_followed_by_zeros() {
        let mut feed = Feed::new(Source::Clip(vec![1, -2, 300]));
        let mut bytes = [0xAA; 5 * 4];
        let flags = pack(&mut bytes, 5, &mut feed);
        assert_eq!(
            frames_of(&bytes),
            [(1, 1), (-2, -2), (300, 300), (0, 0), (0, 0)]
        );
        assert_eq!(flags, 0);
        assert_eq!((feed.cursor, feed.queued), (3, 5));
    }

    #[test]
    fn a_clip_longer_than_the_buffer_goes_out_over_several_buffers() {
        let mut feed = Feed::new(Source::Clip(vec![1, 2, 3, 4, 5]));
        let mut bytes = [0u8; 2 * 4];
        assert_eq!(pack(&mut bytes, 2, &mut feed), 0);
        assert_eq!(frames_of(&bytes), [(1, 1), (2, 2)]);
        assert_eq!(pack(&mut bytes, 2, &mut feed), 0);
        assert_eq!(frames_of(&bytes), [(3, 3), (4, 4)]);
        assert_eq!(pack(&mut bytes, 2, &mut feed), 0);
        assert_eq!(frames_of(&bytes), [(5, 5), (0, 0)]);
        // from here on there is nothing of the clip in a buffer
        assert_eq!(pack(&mut bytes, 2, &mut feed), SILENT);
        assert_eq!(frames_of(&bytes), [(0, 0), (0, 0)]);
        assert_eq!((feed.cursor, feed.queued), (5, 8));
    }

    #[test]
    fn the_silent_stream_is_all_zeros_flagged_as_silence() {
        let mut feed = Feed::new(Source::Silence);
        // a mix format with 8 bytes to the frame, as a float stereo stream has
        let mut bytes = [0xAA; 3 * 8];
        assert_eq!(pack(&mut bytes, 3, &mut feed), SILENT);
        assert!(bytes.iter().all(|&b| b == 0));
        assert_eq!((feed.cursor, feed.queued), (0, 3));
        assert!(!feed.finished(0), "the silence has no end");
    }

    #[test]
    fn a_clip_is_finished_when_its_last_frame_has_been_played() {
        let mut feed = Feed::new(Source::Clip(vec![1, 2, 3]));
        let mut bytes = [0u8; 5 * 4];
        // not all of the clip is in a buffer yet
        let mut short = [0u8; 2 * 4];
        pack(&mut short, 2, &mut feed);
        assert!(!feed.finished(0));
        pack(&mut bytes, 5, &mut feed);
        // 7 frames were handed over, 3 of them the clip: the last clip frame is played once the
        // device holds no more than 4 of the frames
        assert!(!feed.finished(7));
        assert!(!feed.finished(5));
        assert!(feed.finished(4));
        assert!(feed.finished(0));
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
