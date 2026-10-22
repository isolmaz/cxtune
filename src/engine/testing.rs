//! Test doubles for the engine: a backend on the fake dongle and a rig that drives the
//! supervisor with a clock the test advances by hand.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::Backend;
use super::supervisor::Supervisor;
use super::types::{Command, Event, Failure, Phase, Snapshot};
use super::watch::CallWatch;
use crate::config::{self, Config};
use crate::fake::{FakeDevice, FakeStream};
use crate::testdir::TempDir;
use crate::{Error, Result};

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
        }
    }
}

impl Backend for TestBackend {
    type Dev = FakeDevice;
    type Stream = FakeStream;

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
    _dir: TempDir,
    pub config_path: PathBuf,
    now: Rc<Cell<Instant>>,
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

        let backend = TestBackend::new(dev.clone(), present);
        let (present, present_fails, vanish_on_open, opens, on_stream) = (
            Arc::clone(&backend.present),
            Arc::clone(&backend.present_fails),
            Arc::clone(&backend.vanish_on_open),
            Arc::clone(&backend.opens),
            Arc::clone(&backend.on_stream),
        );
        let now = Rc::new(Cell::new(Instant::now()));
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
                let now = Rc::clone(&now);
                move || now.get()
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
        let end = self.now.get() + Duration::from_secs_f64(seconds);
        for _ in 0..10_000 {
            let wake = self.sup.next_wake();
            if wake > end {
                self.now.set(end);
                return;
            }
            self.now.set(wake.max(self.now.get()));
            self.sup.step();
        }
        panic!("the supervisor keeps asking to run at once");
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

    pub fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    /// The settings file as it is on disk.
    pub fn saved(&self) -> Config {
        config::load_from(&self.config_path).unwrap()
    }
}
