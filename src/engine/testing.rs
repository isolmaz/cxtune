//! Test doubles for the engine: a backend on the fake dongle and a rig that drives the
//! supervisor with a clock the test advances by hand.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::supervisor::Supervisor;
use super::types::{Command, Event, Failure, Phase, Snapshot, TestEvent};
use super::watch::CallWatch;
use super::{Backend, Clip, Microphone};
use crate::audio::PLAY_RATE;
use crate::capture::{self, BLOCK_MS, Level};
use crate::config::{self, Config};
use crate::fake::{FakeDevice, FakeMic, FakeStream};
use crate::testdir::TempDir;
use crate::{Error, Result};

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// What the fake microphone delivers.
#[derive(Clone, Debug)]
pub(super) struct MicScript {
    /// The first this long after the stream opened is loud, as the codec's start-up is.
    pub warmup: Duration,
    pub warmup_dbfs: f32,
    /// The level of the noise floor with the codec applying noise reduction, and without.
    pub floor_nr_on_dbfs: f32,
    pub floor_nr_off_dbfs: f32,
    /// The stream delivers nothing.
    pub stall: bool,
    /// The stream has stopped by itself.
    pub fail: bool,
}

impl Default for MicScript {
    fn default() -> Self {
        MicScript {
            warmup: Duration::from_millis(3200),
            warmup_dbfs: -20.0,
            floor_nr_on_dbfs: -60.0,
            floor_nr_off_dbfs: -48.0,
            stall: false,
            fail: false,
        }
    }
}

/// The microphone on the fake dongle: opening it opens the fake codec's capture path, and it
/// delivers one block per [`BLOCK_MS`] of clock time.
pub(super) struct FakeCapture {
    _mic: FakeMic,
    dev: FakeDevice,
    clock: Clock,
    script: Arc<Mutex<MicScript>>,
    opened: Instant,
    produced: Instant,
    recording: bool,
}

impl Microphone for FakeCapture {
    fn poll(&mut self) -> Result<capture::Poll> {
        let script = self.script.lock().unwrap().clone();
        if script.fail {
            return Err(Error::Audio("the microphone stream stopped".into()));
        }
        let now = (self.clock)();
        let mut poll = capture::Poll::default();
        let block = Duration::from_millis(u64::from(BLOCK_MS));
        if script.stall {
            self.produced = now;
            return Ok(poll);
        }
        while self.produced + block <= now {
            self.produced += block;
            let dbfs = if self.produced - self.opened <= script.warmup {
                script.warmup_dbfs
            } else if self.dev.appliers() & 0x80 != 0 {
                script.floor_nr_on_dbfs
            } else {
                script.floor_nr_off_dbfs
            };
            let amplitude = (32768.0 * 10f64.powf(f64::from(dbfs) / 20.0))
                .round()
                .min(32767.0) as i16;
            let samples: Vec<i16> = (0..capture::RATE * BLOCK_MS / 1000)
                .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
                .collect();
            poll.levels.push(Level::of(&samples));
            if self.recording {
                poll.samples.extend(samples);
            }
        }
        Ok(poll)
    }

    fn record(&mut self, on: bool) {
        self.recording = on;
    }
}

/// A clip on the fake dongle's output: it plays for as long as it is long.
pub(super) struct FakePlayback {
    _stream: FakeStream,
    clock: Clock,
    until: Instant,
    fails: Arc<AtomicBool>,
}

impl Clip for FakePlayback {
    fn is_finished(&mut self) -> Result<bool> {
        if self.fails.load(Ordering::SeqCst) {
            return Err(Error::Audio("the playback stream stopped".into()));
        }
        Ok((self.clock)() >= self.until)
    }
}

/// The fake dongle behind the engine's [`Backend`], with switches the test flips.
pub(super) struct TestBackend {
    pub dev: FakeDevice,
    /// Whether the dongle is enumerated.
    pub present: Arc<AtomicBool>,
    /// Enumeration itself fails.
    pub present_fails: Arc<AtomicBool>,
    /// The dongle disappears (and stops answering) the moment it is opened.
    pub vanish_on_open: Arc<AtomicBool>,
    /// Passes that opened the device.
    pub opens: Arc<AtomicUsize>,
    /// Runs when a silent stream is requested, before it is opened.
    pub on_stream: Arc<Mutex<Box<dyn Fn() + Send>>>,
    /// Runs when the dongle is opened, before it is handed out.
    pub on_open: Arc<Mutex<Box<dyn Fn() + Send>>>,
    /// Runs when the connected devices are listed.
    pub on_present: Arc<Mutex<Box<dyn Fn() + Send>>>,
    pub clock: Clock,
    pub mic: Arc<Mutex<MicScript>>,
    /// Opening the microphone fails.
    pub capture_fails: Arc<AtomicBool>,
    /// The clip being played stops by itself.
    pub playback_fails: Arc<AtomicBool>,
    /// Every clip that was started, as handed to the backend.
    pub played: Arc<Mutex<Vec<Vec<i16>>>>,
}

impl TestBackend {
    pub fn new(dev: FakeDevice, present: bool) -> TestBackend {
        TestBackend {
            dev,
            present: Arc::new(AtomicBool::new(present)),
            present_fails: Arc::new(AtomicBool::new(false)),
            vanish_on_open: Arc::new(AtomicBool::new(false)),
            opens: Arc::new(AtomicUsize::new(0)),
            on_stream: Arc::new(Mutex::new(Box::new(|| {}))),
            on_open: Arc::new(Mutex::new(Box::new(|| {}))),
            on_present: Arc::new(Mutex::new(Box::new(|| {}))),
            clock: Arc::new(Instant::now),
            mic: Arc::new(Mutex::new(MicScript::default())),
            capture_fails: Arc::new(AtomicBool::new(false)),
            playback_fails: Arc::new(AtomicBool::new(false)),
            played: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Backend for TestBackend {
    type Dev = FakeDevice;
    type Stream = FakeStream;
    type Capture = FakeCapture;
    type Playback = FakePlayback;

    fn present(&mut self) -> Result<bool> {
        (self.on_present.lock().unwrap())();
        if self.present_fails.load(Ordering::SeqCst) {
            return Err(Error::Device("the device list cannot be read".into()));
        }
        Ok(self.present.load(Ordering::SeqCst))
    }

    fn open(&mut self) -> Result<FakeDevice> {
        (self.on_open.lock().unwrap())();
        self.opens.fetch_add(1, Ordering::SeqCst);
        if self.vanish_on_open.load(Ordering::SeqCst) {
            self.present.store(false, Ordering::SeqCst);
            self.dev.unplug();
        }
        if self.present.load(Ordering::SeqCst) {
            Ok(self.dev.clone())
        } else {
            Err(Error::Device("the dongle is not connected".into()))
        }
    }

    fn open_stream(&mut self) -> Result<FakeStream> {
        (self.on_stream.lock().unwrap())();
        self.dev.open_active_stream()
    }

    fn open_capture(&mut self) -> Result<FakeCapture> {
        if self.capture_fails.load(Ordering::SeqCst) {
            return Err(Error::Audio("no microphone endpoint".into()));
        }
        let now = (self.clock)();
        Ok(FakeCapture {
            _mic: self.dev.open_mic(),
            dev: self.dev.clone(),
            clock: Arc::clone(&self.clock),
            script: Arc::clone(&self.mic),
            opened: now,
            produced: now,
            recording: false,
        })
    }

    fn play(&mut self, samples: Vec<i16>) -> Result<FakePlayback> {
        let stream = self.dev.open_active_stream()?;
        let length = Duration::from_micros(samples.len() as u64 * 1_000_000 / u64::from(PLAY_RATE));
        self.played.lock().unwrap().push(samples);
        Ok(FakePlayback {
            _stream: stream,
            clock: Arc::clone(&self.clock),
            until: (self.clock)() + length,
            fails: Arc::clone(&self.playback_fails),
        })
    }
}

/// A supervisor on a [`TestBackend`] whose clock only moves when the test says so.
pub(super) struct Rig {
    pub sup: Supervisor<TestBackend>,
    pub dev: FakeDevice,
    pub present: Arc<AtomicBool>,
    present_fails: Arc<AtomicBool>,
    pub vanish_on_open: Arc<AtomicBool>,
    pub opens: Arc<AtomicUsize>,
    on_stream: Arc<Mutex<Box<dyn Fn() + Send>>>,
    pub mic: Arc<Mutex<MicScript>>,
    pub capture_fails: Arc<AtomicBool>,
    pub playback_fails: Arc<AtomicBool>,
    pub played: Arc<Mutex<Vec<Vec<i16>>>>,
    _dir: TempDir,
    pub config_path: PathBuf,
    now: Arc<Mutex<Instant>>,
    events: Rc<RefCell<Vec<Event>>>,
    log: Rc<RefCell<Vec<String>>>,
}

impl Rig {
    pub fn new(cfg: Config, present: bool) -> Rig {
        Rig::with_device(FakeDevice::stock(), cfg, present)
    }

    pub fn with_device(dev: FakeDevice, cfg: Config, present: bool) -> Rig {
        let dir = TempDir::new("engine-rig");
        let config_path = dir.join("config.toml");

        let now = Arc::new(Mutex::new(Instant::now()));
        let mut backend = TestBackend::new(dev.clone(), present);
        backend.clock = Arc::new({
            let now = Arc::clone(&now);
            move || *now.lock().unwrap()
        });
        let (mic, capture_fails, playback_fails, played) = (
            Arc::clone(&backend.mic),
            Arc::clone(&backend.capture_fails),
            Arc::clone(&backend.playback_fails),
            Arc::clone(&backend.played),
        );
        let (present, present_fails, vanish_on_open, opens, on_stream) = (
            Arc::clone(&backend.present),
            Arc::clone(&backend.present_fails),
            Arc::clone(&backend.vanish_on_open),
            Arc::clone(&backend.opens),
            Arc::clone(&backend.on_stream),
        );
        let events = Rc::new(RefCell::new(Vec::new()));
        let log = Rc::new(RefCell::new(Vec::new()));
        let sup = Supervisor::new(
            backend,
            cfg,
            config_path.clone(),
            Box::new({
                let events = Rc::clone(&events);
                move |event| events.borrow_mut().push(event)
            }),
            Box::new({
                let log = Rc::clone(&log);
                move |line| log.borrow_mut().push(line.to_string())
            }),
            Box::new({
                let now = Arc::clone(&now);
                move || *now.lock().unwrap()
            }),
            CallWatch::default(),
        );
        Rig {
            sup,
            dev,
            present,
            present_fails,
            vanish_on_open,
            opens,
            on_stream,
            mic,
            capture_fails,
            playback_fails,
            played,
            _dir: dir,
            config_path,
            now,
            events,
            log,
        }
    }

    pub fn set_present(&self, present: bool) {
        self.present.store(present, Ordering::SeqCst);
    }

    pub fn set_enumeration_fails(&self, fails: bool) {
        self.present_fails.store(fails, Ordering::SeqCst);
    }

    pub fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }

    /// Runs `hook` whenever a silent stream is requested, before it is opened.
    pub fn on_stream(&self, hook: impl Fn() + Send + 'static) {
        *self.on_stream.lock().unwrap() = Box::new(hook);
    }

    pub fn send(&mut self, command: Command) {
        self.sup.command(command);
    }

    /// Lets `seconds` of virtual time pass, running the supervisor at every moment it asks to be.
    pub fn run(&mut self, seconds: f64) {
        let end = self.now() + Duration::from_secs_f64(seconds);
        for _ in 0..100_000 {
            let wake = self.sup.next_wake();
            if wake > end {
                *self.now.lock().unwrap() = end;
                return;
            }
            *self.now.lock().unwrap() = wake.max(self.now());
            self.sup.step();
        }
        panic!("the supervisor keeps asking to run at once");
    }

    fn now(&self) -> Instant {
        *self.now.lock().unwrap()
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.borrow().clone()
    }

    pub fn clear_events(&self) {
        self.events.borrow_mut().clear();
    }

    pub fn states(&self) -> Vec<Snapshot> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::State(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    /// The latest state that was published.
    pub fn snapshot(&self) -> Snapshot {
        self.states().pop().expect("a state is published at start")
    }

    pub fn phase(&self) -> Phase {
        self.snapshot().phase
    }

    pub fn errors(&self) -> Vec<Failure> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Error(f) => Some(f),
                _ => None,
            })
            .collect()
    }

    pub fn warnings(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Warning(w) => Some(w),
                _ => None,
            })
            .collect()
    }

    /// The events of the Test tab, in order.
    pub fn test_events(&self) -> Vec<TestEvent> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Test(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    pub fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    /// The settings file as it is on disk.
    pub fn saved(&self) -> Config {
        config::load_from(&self.config_path).unwrap()
    }
}
