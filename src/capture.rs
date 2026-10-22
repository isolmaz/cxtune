//! Capture from the dongle's microphone endpoint, opened on demand.
//!
//! The stream is a shared-mode capture client in 16-bit mono at 48 kHz (the audio engine converts
//! from the endpoint's mix format), so other programs keep using the microphone. While it is open
//! the codec firmware keeps its capture path up, which the output modes take into account (see
//! docs/firmware-notes.md): the stream is closed by dropping the [`Capture`].
//!
//! The stream is cut into blocks of [`BLOCK_MS`] ms; [`Capture::poll`] returns the level of each
//! block that was completed since the last call and, while recording, the samples.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eCapture,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};

use crate::audio;
use crate::protocol::{PID, VID};
use crate::{Error, Result};

/// Sample rate of the capture stream and of the samples it delivers.
pub const RATE: u32 = 48_000;
/// Length of one level block.
pub const BLOCK_MS: u32 = 50;
const BLOCK_FRAMES: usize = (RATE * BLOCK_MS / 1000) as usize;
/// How often the capture buffer is drained.
const DRAIN: Duration = Duration::from_millis(10);
/// Capture buffer length in 100 ns units.
const BUFFER_100NS: i64 = 2_000_000;
/// The longest recording kept, in seconds. Samples beyond it are dropped.
pub const MAX_RECORD_SECONDS: u32 = 60;

/// Level reported for silence and for anything below it.
pub const FLOOR_DBFS: f32 = -120.0;

/// Level of a block of samples in dB relative to full scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Level {
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
}

/// `amplitude` (1.0 is full scale) in dBFS, not lower than [`FLOOR_DBFS`].
fn dbfs(amplitude: f64) -> f32 {
    ((20.0 * amplitude.log10()) as f32).max(FLOOR_DBFS)
}

impl Level {
    /// The RMS and peak level of `samples`; [`FLOOR_DBFS`] for none.
    pub fn of(samples: &[i16]) -> Level {
        if samples.is_empty() {
            return Level {
                rms_dbfs: FLOOR_DBFS,
                peak_dbfs: FLOOR_DBFS,
            };
        }
        let full = 32768.0;
        let sum: f64 = samples.iter().map(|&s| (f64::from(s) / full).powi(2)).sum();
        let peak = samples
            .iter()
            .map(|&s| f64::from(s).abs() / full)
            .fold(0.0, f64::max);
        Level {
            rms_dbfs: dbfs((sum / samples.len() as f64).sqrt()),
            peak_dbfs: dbfs(peak),
        }
    }
}

/// The median of the RMS levels in dBFS (the mean of the two middle ones for an even count);
/// `None` for no levels. This is the noise floor of a stationary signal: a click or a cough in
/// a few blocks does not move it.
pub fn median_rms_dbfs(levels: &[Level]) -> Option<f32> {
    let mut rms: Vec<f32> = levels.iter().map(|l| l.rms_dbfs).collect();
    rms.sort_by(f32::total_cmp);
    let n = rms.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(rms[n / 2]),
        _ => Some((rms[n / 2 - 1] + rms[n / 2]) / 2.0),
    }
}

/// What the capture thread has collected since the last [`Capture::poll`].
#[derive(Debug, Default, PartialEq)]
pub struct Poll {
    /// The level of every block completed since the last call, oldest first.
    pub levels: Vec<Level>,
    /// The samples of those blocks, only while recording.
    pub samples: Vec<i16>,
}

#[derive(Default)]
struct Shared {
    poll: Poll,
    recording: bool,
    /// Samples kept in the current recording.
    kept: usize,
    failure: Option<String>,
}

/// The open capture stream. Dropping it closes the stream.
pub struct Capture {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Mutex<Shared>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Capture {
    /// Starts or ends keeping the samples. Starting begins a new recording, so samples of an
    /// earlier one that were not polled are dropped.
    pub fn record(&self, on: bool) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.recording = on;
        shared.kept = 0;
        shared.poll.samples.clear();
    }

    /// Returns what has been collected since the last call. An error if the stream stopped by
    /// itself.
    pub fn poll(&self) -> Result<Poll> {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(why) = &shared.failure {
            return Err(Error::Audio(format!("the capture stream stopped: {why}")));
        }
        Ok(std::mem::take(&mut shared.poll))
    }
}

/// Opens the capture stream on the dongle's microphone endpoint and returns once Windows is
/// running it.
pub fn open() -> Result<Capture> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let thread = {
        let shared = Arc::clone(&shared);
        thread::spawn(move || capture(&started_tx, &stop_rx, &shared))
    };
    let stream = Capture {
        stop: Some(stop_tx),
        thread: Some(thread),
        shared,
    };
    match started_rx.recv() {
        Ok(Ok(())) => Ok(stream),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::Audio("the capture thread ended unexpectedly".into())),
    }
}

fn audio_err(e: windows::core::Error) -> Error {
    Error::Audio(e.to_string())
}

fn capture(started: &SyncSender<Result<()>>, stop: &Receiver<()>, shared: &Mutex<Shared>) {
    // SAFETY: COM is initialised on this thread before any COM call and uninitialised after
    // every COM object created by `run` has been dropped.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if let Err(e) = init.ok() {
        let _ = started.send(Err(audio_err(e)));
        return;
    }
    if let Err(e) = run(started, stop, shared) {
        let _ = started.send(Err(e));
    }
    unsafe { CoUninitialize() };
}

fn run(
    started: &SyncSender<Result<()>>,
    stop: &Receiver<()>,
    shared: &Mutex<Shared>,
) -> Result<()> {
    // SAFETY: plain WASAPI calls on objects created here; every buffer returned by GetBuffer is
    // copied and released before the next call.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(audio_err)?;
        let device = audio::find_dongle(&enumerator, eCapture)?.ok_or_else(|| {
            Error::Audio(format!(
                "no microphone endpoint of the dongle ({VID:04X}:{PID:04X}) found"
            ))
        })?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(audio_err)?;
        let format = pcm16_mono(RATE);
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
        let reader: IAudioCaptureClient = client.GetService().map_err(audio_err)?;
        client.Start().map_err(audio_err)?;
        let _ = started.send(Ok(()));
        let mut blocks = Blocks::default();
        let ended = loop {
            match stop.recv_timeout(DRAIN) {
                Err(RecvTimeoutError::Timeout) => {
                    if let Err(e) = drain(&reader, &mut blocks, shared) {
                        break Err(e);
                    }
                }
                _ => break Ok(()),
            }
        };
        if let Err(e) = ended {
            shared.lock().unwrap_or_else(|e| e.into_inner()).failure = Some(e.to_string());
        }
        let _ = client.Stop();
        Ok(())
    }
}

fn pcm16_mono(rate: u32) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 1,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * 2,
        nBlockAlign: 2,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// The block being filled.
#[derive(Default)]
struct Blocks(Vec<i16>);

impl Blocks {
    /// Adds samples; every completed block goes to `shared`.
    fn push(&mut self, samples: &[i16], shared: &Mutex<Shared>) {
        let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
        for &sample in samples {
            self.0.push(sample);
            if self.0.len() == BLOCK_FRAMES {
                shared.poll.levels.push(Level::of(&self.0));
                if shared.recording {
                    let room = (MAX_RECORD_SECONDS * RATE) as usize - shared.kept;
                    let keep = self.0.len().min(room);
                    shared.poll.samples.extend_from_slice(&self.0[..keep]);
                    shared.kept += keep;
                }
                self.0.clear();
            }
        }
    }
}

unsafe fn drain(
    reader: &IAudioCaptureClient,
    blocks: &mut Blocks,
    shared: &Mutex<Shared>,
) -> Result<()> {
    unsafe {
        while reader.GetNextPacketSize().map_err(audio_err)? > 0 {
            let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0, 0);
            reader
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                .map_err(audio_err)?;
            let samples = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                vec![0i16; frames as usize]
            } else {
                std::slice::from_raw_parts(data as *const i16, frames as usize).to_vec()
            };
            reader.ReleaseBuffer(frames).map_err(audio_err)?;
            blocks.push(&samples, shared);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amplitude: f64, count: usize) -> Vec<i16> {
        (0..count)
            .map(|i| {
                let phase = 2.0 * std::f64::consts::PI * 1000.0 * i as f64 / f64::from(RATE);
                (amplitude * 32767.0 * phase.sin()).round() as i16
            })
            .collect()
    }

    #[test]
    fn a_full_scale_sine_peaks_at_0_dbfs_and_has_an_rms_3_db_lower() {
        let level = Level::of(&sine(1.0, BLOCK_FRAMES));
        assert!(level.peak_dbfs.abs() < 0.01, "{level:?}");
        assert!((level.rms_dbfs + 3.01).abs() < 0.02, "{level:?}");
    }

    #[test]
    fn a_sine_of_a_tenth_is_20_dbfs_below_full_scale() {
        let level = Level::of(&sine(0.1, BLOCK_FRAMES));
        assert!((level.peak_dbfs + 20.0).abs() < 0.01, "{level:?}");
        assert!((level.rms_dbfs + 23.01).abs() < 0.02, "{level:?}");
    }

    #[test]
    fn silence_and_nothing_are_at_the_floor() {
        for samples in [vec![0i16; 100], vec![]] {
            let level = Level::of(&samples);
            assert_eq!(level.rms_dbfs, FLOOR_DBFS);
            assert_eq!(level.peak_dbfs, FLOOR_DBFS);
        }
    }

    #[test]
    fn the_most_negative_sample_is_full_scale() {
        assert_eq!(Level::of(&[i16::MIN]).peak_dbfs, 0.0);
    }

    fn at(rms_dbfs: f32) -> Level {
        Level {
            rms_dbfs,
            peak_dbfs: rms_dbfs,
        }
    }

    #[test]
    fn the_median_ignores_a_few_loud_blocks() {
        let mut levels: Vec<Level> = [-64.0, -63.0, -65.0, -64.5, -63.5]
            .into_iter()
            .map(at)
            .collect();
        assert_eq!(median_rms_dbfs(&levels), Some(-64.0));
        levels.push(at(-1.0));
        levels.push(at(-1.0));
        assert_eq!(median_rms_dbfs(&levels), Some(-63.5));
    }

    #[test]
    fn an_even_count_takes_the_mean_of_the_middle_two() {
        let levels: Vec<Level> = [-60.0, -50.0, -70.0, -40.0].into_iter().map(at).collect();
        assert_eq!(median_rms_dbfs(&levels), Some(-55.0));
        assert_eq!(median_rms_dbfs(&[]), None);
    }

    #[test]
    fn blocks_are_cut_at_50_ms_and_samples_are_kept_only_while_recording() {
        let shared = Mutex::new(Shared::default());
        let mut blocks = Blocks::default();
        // two and a half blocks, in uneven pieces
        let samples = sine(0.1, BLOCK_FRAMES * 5 / 2);
        for piece in samples.chunks(1000) {
            blocks.push(piece, &shared);
        }
        let first = std::mem::take(&mut shared.lock().unwrap().poll);
        assert_eq!(first.levels.len(), 2);
        assert!(first.samples.is_empty());

        shared.lock().unwrap().recording = true;
        blocks.push(&sine(0.1, BLOCK_FRAMES), &shared);
        let second = std::mem::take(&mut shared.lock().unwrap().poll);
        // the half block that was already there completes the first block of the recording
        assert_eq!(second.levels.len(), 1);
        assert_eq!(second.samples.len(), BLOCK_FRAMES);
    }

    #[test]
    fn a_recording_stops_growing_at_the_limit() {
        let shared = Mutex::new(Shared {
            recording: true,
            kept: (MAX_RECORD_SECONDS * RATE) as usize - BLOCK_FRAMES / 2,
            ..Shared::default()
        });
        let mut blocks = Blocks::default();
        blocks.push(&vec![1i16; BLOCK_FRAMES * 2], &shared);
        let poll = std::mem::take(&mut shared.lock().unwrap().poll);
        assert_eq!(poll.levels.len(), 2);
        assert_eq!(poll.samples.len(), BLOCK_FRAMES / 2);
    }
}
