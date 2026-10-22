//! The background engine: one worker thread that owns the dongle and keeps it at the saved
//! settings, for a user interface (or `cxtune watch`) to drive.
//!
//! - **One owner.** [`Engine::start`] takes the machine-wide device lock ([`NamedLock::device`])
//!   and holds it until the engine stops. The worker thread is the only code that opens the HID
//!   device or the silent playback stream.
//! - **Commands in, events out.** [`Handle::send`] never blocks and can be used from any
//!   thread. The sink given to [`Engine::start`] receives [`Event`]s on the worker thread; it
//!   must return quickly (a user interface forwards them to its own thread).
//! - **Coalescing.** Commands that arrive while the worker is busy, or within a short window of
//!   each other, are merged: only the latest value of each setting is written.
//! - **Failure.** A panic on the worker thread ends the engine with a last [`Event::Error`] and a
//!   state in [`Phase::Stopped`]; [`Engine::is_running`] turns false. A call into the device that
//!   never returns is noticed by a watchdog thread, reported as [`ErrorKind::Unresponsive`], and
//!   does not keep [`Engine::stop`] waiting.
//!
//! The decisions live in [`supervisor`]; this module is the threads around it and the two
//! environments it can run in: the real dongle ([`HidBackend`]) and the tests' fake.

mod supervisor;
mod types;
mod watch;

#[cfg(test)]
mod testing;

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use hidapi::HidApi;
use supervisor::Supervisor;
pub use types::{Command, ErrorKind, Event, Failure, Phase, Readback, Snapshot};
use watch::CallWatch;

use crate::lock::NamedLock;
use crate::log::{self, Log};
use crate::mode::Stream;
use crate::protocol::Transport;
use crate::{Error, Result, audio, config, hid};

/// Everything the engine needs from the world around the dongle.
pub trait Backend {
    type Dev: Transport;
    type Stream: Stream;

    /// Whether the dongle is connected. Enumeration only: no device I/O.
    fn present(&mut self) -> Result<bool>;
    /// Opens the dongle for one pass of reads and writes.
    fn open(&mut self) -> Result<Self::Dev>;
    /// Starts a playback stream of digital silence on the dongle's output.
    fn open_stream(&mut self) -> Result<Self::Stream>;
}

/// The real dongle: HID for the memory protocol, WASAPI for the silent stream.
pub struct HidBackend {
    api: HidApi,
}

impl HidBackend {
    pub fn new() -> Result<HidBackend> {
        Ok(HidBackend { api: hid::api()? })
    }
}

impl Backend for HidBackend {
    type Dev = hid::HidTransport;
    type Stream = audio::SilentStream;

    fn present(&mut self) -> Result<bool> {
        hid::refresh(&mut self.api)?;
        Ok(hid::find(&self.api).is_some())
    }

    fn open(&mut self) -> Result<hid::HidTransport> {
        hid::refresh(&mut self.api)?;
        hid::open(&self.api)
    }

    fn open_stream(&mut self) -> Result<audio::SilentStream> {
        audio::open_silence()
    }
}

enum Message {
    Command(Command),
    Stop,
}

/// Sends commands to the engine. Cheap to clone; usable from any thread.
#[derive(Clone)]
pub struct Handle {
    tx: Sender<Message>,
}

impl Handle {
    /// Queues a command. It is dropped if the engine has stopped.
    pub fn send(&self, command: Command) {
        let _ = self.tx.send(Message::Command(command));
    }
}

/// How long a call into the device may run before the dongle counts as not answering. A request
/// takes tens of milliseconds and the slowest legitimate call (opening the audio stream) a second
/// or two.
const STALL: Duration = Duration::from_secs(10);
/// How long [`Engine::stop`] waits for the worker to finish what it is doing: longer than the
/// slowest mode change, which waits up to [`crate::mode::READY_TIMEOUT`] for the codec.
const STOP_WAIT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy)]
struct Limits {
    stall: Duration,
    stop_wait: Duration,
}

const LIMITS: Limits = Limits {
    stall: STALL,
    stop_wait: STOP_WAIT,
};

/// The sink, shared by the worker and the watchdog, with the latest state that went through it
/// (the watchdog repeats it with the problem added).
struct Hub {
    sink: Box<dyn FnMut(Event) + Send>,
    last: Option<Snapshot>,
}

impl Hub {
    fn emit(&mut self, event: Event) {
        if let Event::State(snapshot) = &event {
            self.last = Some(snapshot.clone());
        }
        (self.sink)(event);
    }
}

fn emit(hub: &Mutex<Hub>, event: Event) {
    hub.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .emit(event);
}

/// A running engine. Dropping it stops it: the worker finishes what it is doing, saves the
/// settings and releases the device lock.
pub struct Engine {
    handle: Handle,
    worker: Option<JoinHandle<()>>,
    watchdog: Option<(Sender<()>, JoinHandle<()>)>,
    watch: CallWatch,
    stop_wait: Duration,
}

impl Engine {
    /// Starts the engine on the real dongle, with the settings from `%APPDATA%\cxtune` and a
    /// diagnostic log in `%LOCALAPPDATA%\cxtune`.
    ///
    /// Fails with [`Error::Busy`] if another cxtune process controls the dongle, and with
    /// [`Error::Config`] if the settings file cannot be read. Nothing has touched the dongle
    /// by then. The first [`Event::State`] follows the first enumeration of the devices.
    pub fn start(sink: impl FnMut(Event) + Send + 'static) -> Result<Engine> {
        Engine::start_with(config::path()?, log::default_path(), HidBackend::new, sink)
    }

    /// [`Engine::start`] on any [`Backend`], with the settings file and the log at the given
    /// places. The device lock is taken all the same. The backend is made on the worker thread,
    /// so it need not be `Send`; a failure to make it is returned here.
    pub fn start_with<B: Backend + 'static>(
        config_path: PathBuf,
        log_path: Option<PathBuf>,
        make_backend: impl FnOnce() -> Result<B> + Send + 'static,
        sink: impl FnMut(Event) + Send + 'static,
    ) -> Result<Engine> {
        let lock = NamedLock::device()?;
        let log = log_path.map(|path| Log::open(path, log::DEFAULT_LIMIT));
        Engine::launch(
            lock,
            config_path,
            log,
            make_backend,
            sink,
            Box::new(SystemTime::now),
            LIMITS,
        )
    }

    /// Starts the worker thread. The worker holds `lock` until it ends; the tests give it a lock
    /// of their own, a replaceable wall clock (which notices sleep) and short limits.
    fn launch<B: Backend + 'static>(
        lock: NamedLock,
        config_path: PathBuf,
        log: Option<Log>,
        make_backend: impl FnOnce() -> Result<B> + Send + 'static,
        sink: impl FnMut(Event) + Send + 'static,
        wall_clock: Box<dyn Fn() -> SystemTime + Send>,
        limits: Limits,
    ) -> Result<Engine> {
        let cfg = config::load_from(&config_path)?;
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let hub = Arc::new(Mutex::new(Hub {
            sink: Box::new(sink),
            last: None,
        }));
        let watch = CallWatch::default();
        let worker = thread::Builder::new()
            .name("cxtune-engine".into())
            .spawn({
                let (hub, watch) = (Arc::clone(&hub), watch.clone());
                move || {
                    let _lock = lock;
                    let backend = match make_backend() {
                        Ok(backend) => backend,
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(()));
                    let mut log = log;
                    let mut supervisor = None;
                    let outcome = catch_unwind(AssertUnwindSafe(|| {
                        let supervisor = supervisor.insert(Supervisor::new(
                            backend,
                            cfg,
                            config_path,
                            Box::new({
                                let hub = Arc::clone(&hub);
                                move |event| emit(&hub, event)
                            }),
                            Box::new(move |message| {
                                if let Some(log) = &mut log {
                                    log.line(message);
                                }
                            }),
                            Box::new(Instant::now),
                            watch,
                        ));
                        run(supervisor, &rx, &*wall_clock);
                    }));
                    if let Err(payload) = outcome {
                        let failure = Failure {
                            kind: ErrorKind::Fatal,
                            message: format!(
                                "the engine stopped after an internal error ({}); restart the \
                                 application",
                                panic_text(&*payload)
                            ),
                        };
                        // the sink may be what panicked, so this is tried once and not trusted
                        let _ = catch_unwind(AssertUnwindSafe(|| match supervisor.as_mut() {
                            Some(supervisor) => supervisor.stopped(failure),
                            None => emit(&hub, Event::Error(failure)),
                        }));
                    }
                }
            })
            .map_err(|e| Error::Device(format!("could not start the engine thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = worker.join();
                return Err(error);
            }
            Err(_) => {
                let _ = worker.join();
                return Err(Error::Device("the engine thread ended unexpectedly".into()));
            }
        }
        let mut engine = Engine {
            handle: Handle { tx },
            worker: Some(worker),
            watchdog: None,
            watch: watch.clone(),
            stop_wait: limits.stop_wait,
        };
        let (stop_tx, stop_rx) = mpsc::channel();
        let watchdog = thread::Builder::new()
            .name("cxtune-watchdog".into())
            .spawn(move || watchdog(&watch, &hub, limits.stall, &stop_rx))
            .map_err(|e| Error::Device(format!("could not start the watchdog thread: {e}")))?;
        engine.watchdog = Some((stop_tx, watchdog));
        Ok(engine)
    }

    /// A sender for commands that can be handed to other threads.
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Whether the worker thread is still working. False after [`Engine::stop`] and after a
    /// failure that ended the engine ([`Phase::Stopped`]).
    pub fn is_running(&self) -> bool {
        self.worker.as_ref().is_some_and(|w| !w.is_finished())
    }

    /// Stops the engine: the worker finishes what it is doing, saves the settings and releases
    /// the device lock. Waits for that for at most 20 s, and not at all while a call into the
    /// device is stuck; the worker is then left behind, still holding the lock, until the
    /// process ends or the call returns. A user interface calls this off its own thread, because
    /// finishing a mode change takes seconds.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.handle.tx.send(Message::Stop);
            let started = Instant::now();
            while !worker.is_finished()
                && !self.watch.is_stuck()
                && started.elapsed() < self.stop_wait
            {
                thread::sleep(Duration::from_millis(10));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
        if let Some((stop, watchdog)) = self.watchdog.take() {
            drop(stop);
            let _ = watchdog.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn panic_text(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| text.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".into())
}

/// Reports a call into the device that does not return, until the engine stops.
fn watchdog(watch: &CallWatch, hub: &Mutex<Hub>, limit: Duration, stop: &Receiver<()>) {
    let tick = (limit / 4).clamp(Duration::from_millis(5), Duration::from_millis(500));
    while stop.recv_timeout(tick) == Err(RecvTimeoutError::Timeout) {
        let Some((what, running)) = watch.stuck(limit) else {
            continue;
        };
        let failure = Failure {
            kind: ErrorKind::Unresponsive,
            message: format!(
                "the dongle does not answer: {what} has not returned for {} s. Unplug and \
                 replug the dongle; the engine goes on by itself once the request ends",
                running.as_secs()
            ),
        };
        let mut hub = hub.lock().unwrap_or_else(PoisonError::into_inner);
        hub.emit(Event::Error(failure.clone()));
        if let Some(mut state) = hub.last.clone() {
            state.error = Some(failure);
            hub.emit(Event::State(state));
        }
    }
}

/// A gap this long between two passes of the worker loop means the computer was asleep: the loop
/// wakes at least once per poll interval.
const RESUME_GAP: Duration = Duration::from_secs(5);

/// Notices that the computer slept by comparing wall-clock time with the loop's own rhythm. The
/// monotonic clock may or may not run during sleep; the wall clock always does. A clock that was
/// set forward looks the same and only costs one extra verification.
struct ResumeDetector {
    last: SystemTime,
}

impl ResumeDetector {
    fn new(now: SystemTime) -> ResumeDetector {
        ResumeDetector { last: now }
    }

    fn resumed(&self, now: SystemTime) -> bool {
        now.duration_since(self.last)
            .is_ok_and(|gap| gap > RESUME_GAP)
    }

    /// Marks the end of a pass. Called after the pass's own work, so that a long write sequence
    /// is not mistaken for sleep.
    fn seen(&mut self, now: SystemTime) {
        self.last = now;
    }
}

fn run<B: Backend>(
    supervisor: &mut Supervisor<B>,
    rx: &Receiver<Message>,
    wall_clock: &dyn Fn() -> SystemTime,
) {
    let mut detector = ResumeDetector::new(wall_clock());
    'work: loop {
        let wait = supervisor
            .next_wake()
            .saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Message::Command(command)) => supervisor.command(command),
            Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        // everything that arrived meanwhile is taken before any of it is acted on
        while let Ok(message) = rx.try_recv() {
            match message {
                Message::Command(command) => supervisor.command(command),
                Message::Stop => break 'work,
            }
        }
        if detector.resumed(wall_clock()) {
            supervisor.resumed();
        }
        supervisor.step();
        detector.seen(wall_clock());
    }
    supervisor.finish();
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::testing::TestBackend;
    use super::*;
    use crate::config::Config;
    use crate::fake::FakeDevice;
    use crate::mode::Mode;
    use crate::testdir::TempDir;

    fn lock_name(tag: &str) -> String {
        format!("Local\\cxtune-test-engine-{tag}-{}", std::process::id())
    }

    /// Starts an engine on a lock of the test's own, with the real clock and limits.
    fn start_locked<B: Backend + 'static>(
        name: &str,
        config_path: PathBuf,
        make_backend: impl FnOnce() -> Result<B> + Send + 'static,
    ) -> Result<Engine> {
        Engine::launch(
            NamedLock::acquire(name).unwrap(),
            config_path,
            None,
            make_backend,
            |_| {},
            Box::new(SystemTime::now),
            LIMITS,
        )
    }

    /// An engine on the fake dongle, with its events collected on a channel.
    struct Running {
        engine: Option<Engine>,
        events: Receiver<Event>,
        /// States taken from the channel so far.
        consumed: std::cell::Cell<usize>,
        /// Errors taken from the channel so far.
        errors: std::cell::RefCell<Vec<Failure>>,
        dev: FakeDevice,
        dir: TempDir,
    }

    impl Running {
        fn start(tag: &str, config: &str, present: bool) -> Running {
            Running::start_with(tag, config, present, |_| {})
        }

        fn start_with(
            tag: &str,
            config: &str,
            present: bool,
            prepare: impl FnOnce(&TestBackend),
        ) -> Running {
            Running::launch(tag, config, present, prepare, Box::new(SystemTime::now))
        }

        fn launch(
            tag: &str,
            config: &str,
            present: bool,
            prepare: impl FnOnce(&TestBackend),
            wall_clock: Box<dyn Fn() -> SystemTime + Send>,
        ) -> Running {
            Running::build(tag, config, present, prepare, wall_clock, LIMITS)
        }

        fn build(
            tag: &str,
            config: &str,
            present: bool,
            prepare: impl FnOnce(&TestBackend),
            wall_clock: Box<dyn Fn() -> SystemTime + Send>,
            limits: Limits,
        ) -> Running {
            let dir = TempDir::new(&format!("engine-{tag}"));
            std::fs::write(dir.join("config.toml"), config).unwrap();
            let dev = FakeDevice::stock();
            let backend = TestBackend::new(dev.clone(), present);
            prepare(&backend);
            let (tx, events) = mpsc::channel();
            let engine = Engine::launch(
                NamedLock::acquire(&lock_name(tag)).unwrap(),
                dir.join("config.toml"),
                Some(Log::open(dir.join("cxtune.log"), log::DEFAULT_LIMIT)),
                move || Ok(backend),
                move |event| {
                    let _ = tx.send(event);
                },
                wall_clock,
                limits,
            )
            .unwrap_or_else(|e| panic!("{e}"));
            Running {
                engine: Some(engine),
                events,
                consumed: std::cell::Cell::new(0),
                errors: std::cell::RefCell::new(Vec::new()),
                dev,
                dir,
            }
        }

        /// The first state that `matches`, within a generous real-time limit.
        fn wait_for(&self, what: &str, matches: impl Fn(&Snapshot) -> bool) -> Snapshot {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut seen = Vec::new();
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.events.recv_timeout(left) {
                    Ok(Event::State(state)) if matches(&state) => {
                        self.consumed.set(self.consumed.get() + 1);
                        return state;
                    }
                    Ok(Event::State(state)) => {
                        self.consumed.set(self.consumed.get() + 1);
                        seen.push(state);
                    }
                    Ok(Event::Error(failure)) => self.errors.borrow_mut().push(failure),
                    Ok(Event::Warning(_)) => {}
                    Err(_) => panic!("timed out waiting for {what}; states seen: {seen:#?}"),
                }
            }
        }

        /// The first error of `kind`, within a generous real-time limit.
        fn wait_for_error(&self, kind: ErrorKind) -> Failure {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.events.recv_timeout(left) {
                    Ok(Event::Error(failure)) if failure.kind == kind => return failure,
                    Ok(_) => {}
                    Err(_) => panic!("timed out waiting for an error of kind {kind}"),
                }
            }
        }

        fn in_sync(&self) -> Snapshot {
            self.wait_for("a connected dongle that holds the saved settings", |s| {
                s.phase == Phase::Connected && s.in_sync()
            })
        }

        fn saved(&self) -> Config {
            config::load_from(&self.dir.join("config.toml")).unwrap()
        }

        fn handle(&self) -> Handle {
            self.engine.as_ref().unwrap().handle()
        }

        fn stop(&mut self) {
            self.engine.take().unwrap().stop();
        }
    }

    fn writes_to(dev: &FakeDevice, addr: u16) -> Vec<Vec<u8>> {
        dev.writes()
            .into_iter()
            .filter(|(a, _)| *a == addr)
            .map(|(_, data)| data)
            .collect()
    }

    #[test]
    fn the_engine_brings_a_connected_dongle_to_the_saved_settings_and_logs_it() {
        let mut running = Running::start("bring", "mode = \"balanced\"\nmic_boost = 6\n", true);
        let first = running.wait_for("the first state", |_| true);
        assert_eq!(first.phase, Phase::Waiting);
        assert_eq!(first.settings.mode, Some(Mode::Balanced));

        let state = running.in_sync();
        assert_eq!(state.device.unwrap().mic_boost, Some(6));
        let mut expected = FakeDevice::in_mode(Mode::Balanced).mem();
        expected[0x1017] = 0x3E;
        assert_eq!(running.dev.mem(), expected);

        running.stop();
        let log = std::fs::read_to_string(running.dir.join("cxtune.log")).unwrap();
        for text in [
            "engine started",
            "the dongle connected",
            "applying the saved settings",
            "engine stopped",
        ] {
            assert!(log.contains(text), "{text} missing from:\n{log}");
        }
    }

    #[test]
    fn commands_sent_through_a_handle_from_another_thread_are_applied_and_saved() {
        let running = Running::start("handle", "mode = \"balanced\"\n", true);
        running.in_sync();
        let handle = running.handle();
        std::thread::spawn(move || {
            handle.send(Command::SetMicBoost(7));
            handle.send(Command::SetNoiseReduction(false));
            handle.send(Command::SetMode(Mode::Quiet));
        })
        .join()
        .unwrap();

        let state = running.wait_for("the new settings", |s| {
            s.phase == Phase::Connected && s.in_sync() && s.settings.mic_boost == Some(7)
        });
        let device = state.device.unwrap();
        assert_eq!(device.mode, Some(Mode::Quiet));
        assert!(!device.noise_reduction);
        assert_eq!(running.dev.byte(0x1017), 0x3F);

        // the settings file is written before the dongle
        let saved = running.saved();
        assert_eq!(
            (saved.mode, saved.mic_boost, saved.mic_nr),
            (Some(Mode::Quiet), Some(7), Some(false))
        );
    }

    #[test]
    fn commands_that_arrive_while_the_worker_is_busy_are_merged() {
        // the first silent stream blocks until the test lets it go
        let (reached_tx, reached_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        let first = AtomicBool::new(true);
        let running = Running::start_with(
            "busy",
            "mode = \"balanced\"\nmic_boost = 6\n",
            true,
            |backend| {
                *backend.on_stream.lock().unwrap() = Box::new(move || {
                    if first.swap(false, Ordering::SeqCst) {
                        let _ = reached_tx.send(());
                        let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(30));
                    }
                });
            },
        );
        reached_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the engine opens a stream");

        // the worker is inside the mode change: these queue up
        running.wait_for("the mode change", |s| s.phase == Phase::Applying);
        let handle = running.handle();
        for level in 1..=200 {
            handle.send(Command::SetSidetoneLevel(level));
        }
        handle.send(Command::SetMicBoost(7));
        handle.send(Command::SetMode(Mode::Quiet));
        handle.send(Command::SetMode(Mode::Balanced));
        go_tx.send(()).unwrap();

        let before = running.consumed.get();
        let state = running.wait_for("the merged settings", |s| {
            s.phase == Phase::Connected && s.in_sync() && s.settings.sidetone_level == Some(200)
        });
        assert_eq!(state.settings.mic_boost, Some(7));
        // the queued commands were taken together: a few states, not one per command
        let published = running.consumed.get() - before;
        assert!(published <= 6, "{published} states for 203 commands");
        // one mode change, one write of each merged setting
        assert_eq!(running.dev.streams_opened(), 1);
        assert_eq!(writes_to(&running.dev, 0x1276), [vec![200, 0]]);
        assert_eq!(writes_to(&running.dev, 0x1017), [vec![0x3E], vec![0x3F]]);
    }

    #[test]
    fn a_wake_from_sleep_makes_the_engine_leave_the_dongle_alone_and_check_it() {
        let skipped = Arc::new(AtomicU64::new(0));
        let wall_clock = {
            let skipped = Arc::clone(&skipped);
            Box::new(move || {
                SystemTime::now() + Duration::from_secs(skipped.load(Ordering::SeqCst))
            })
        };
        let running = Running::launch("resume", "mode = \"balanced\"\n", true, |_| {}, wall_clock);
        running.in_sync();

        // the dongle restarted while the computer slept, without the engine seeing it go
        running.dev.replug();
        skipped.store(3600, Ordering::SeqCst);
        running.wait_for("the wait after waking", |s| s.phase == Phase::Waiting);
        running.in_sync();
        assert_eq!(running.dev.streams_opened(), 2);
    }

    #[test]
    fn the_engine_holds_the_device_lock_until_it_has_stopped() {
        let name = lock_name("holds");
        let dir = TempDir::new("engine-holds");
        let backend = TestBackend::new(FakeDevice::stock(), false);
        let engine = start_locked(&name, dir.join("config.toml"), move || Ok(backend)).unwrap();
        assert!(engine.is_running());
        assert!(matches!(NamedLock::acquire(&name), Err(Error::Busy(_))));
        engine.stop();
        drop(NamedLock::acquire(&name).unwrap());

        // dropping an engine releases the lock in the same way
        let backend = TestBackend::new(FakeDevice::stock(), false);
        let engine = start_locked(&name, dir.join("config.toml"), move || Ok(backend)).unwrap();
        assert!(matches!(NamedLock::acquire(&name), Err(Error::Busy(_))));
        drop(engine);
        drop(NamedLock::acquire(&name).unwrap());
    }

    #[test]
    fn a_start_that_fails_releases_the_lock() {
        let name = lock_name("fails");
        let dir = TempDir::new("engine-fails");

        // a settings file from an earlier version is rejected before anything runs
        std::fs::write(dir.join("config.toml"), "profile = \"idle\"\n").unwrap();
        let backend = TestBackend::new(FakeDevice::stock(), true);
        let result = start_locked(&name, dir.join("config.toml"), move || Ok(backend));
        let Err(Error::Config(message)) = result else {
            panic!("the start should fail on the settings file")
        };
        assert!(message.contains("profile"), "{message}");
        drop(NamedLock::acquire(&name).unwrap());

        // so is a backend that cannot be made
        std::fs::remove_file(dir.join("config.toml")).unwrap();
        let result = start_locked(&name, dir.join("config.toml"), || -> Result<TestBackend> {
            Err(Error::Device("no HID".into()))
        });
        assert!(matches!(result, Err(Error::Device(_))));
        drop(NamedLock::acquire(&name).unwrap());
    }

    #[test]
    fn the_engine_will_not_start_on_the_real_dongle_while_the_device_lock_is_held() {
        // Either this test holds the lock or a running cxtune does; both make it busy. Nothing
        // here can reach the real dongle: the start fails at the lock.
        let _held = NamedLock::device();
        assert!(matches!(NamedLock::device(), Err(Error::Busy(_))));
        assert!(matches!(Engine::start(|_| {}), Err(Error::Busy(_))));
    }

    #[test]
    fn a_pause_in_the_worker_loop_is_noticed_as_sleep_only_when_it_is_long() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut detector = ResumeDetector::new(start);
        assert!(!detector.resumed(start + POLL_BOUND));
        assert!(!detector.resumed(start + RESUME_GAP));
        assert!(detector.resumed(start + RESUME_GAP + Duration::from_millis(1)));

        // the gap is measured from the end of the last pass
        detector.seen(start + Duration::from_secs(100));
        assert!(!detector.resumed(start + Duration::from_secs(104)));
        assert!(detector.resumed(start + Duration::from_secs(106)));

        // a clock that was set back is not a sleep
        assert!(!detector.resumed(start));
    }

    /// Waits (in real time) until `done`.
    fn eventually(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_panic_on_the_worker_ends_the_engine_with_a_last_error_and_state() {
        let mut running = Running::start_with("panic", "mode = \"quiet\"\n", true, |backend| {
            *backend.on_stream.lock().unwrap() = Box::new(|| panic!("the stream broke"));
        });
        assert!(running.engine.as_ref().unwrap().is_running());
        let state = running.wait_for("the last state", |s| s.phase == Phase::Stopped);
        let error = state.error.expect("the last state says why");
        assert_eq!(error.kind, ErrorKind::Fatal);
        assert!(
            error.message.contains("the stream broke"),
            "{}",
            error.message
        );
        assert_eq!(*running.errors.borrow(), [error]);

        let name = lock_name("panic");
        eventually("the worker to end", || {
            !running.engine.as_ref().unwrap().is_running()
        });
        // the lock is free again, and sending and stopping do no harm
        drop(NamedLock::acquire(&name).unwrap());
        running.handle().send(Command::SetMicBoost(7));
        running.stop();
    }

    #[test]
    fn a_panic_in_the_sink_ends_the_engine_but_the_unsaved_settings_are_written() {
        let name = lock_name("sink");
        let dir = TempDir::new("engine-sink");
        let (tx, rx) = mpsc::channel();
        let backend = TestBackend::new(FakeDevice::stock(), false);
        let engine = Engine::launch(
            NamedLock::acquire(&name).unwrap(),
            dir.join("config.toml"),
            None,
            move || Ok(backend),
            move |event| {
                // the first state that holds the new boost before it is on disk
                if let Event::State(s) = &event
                    && s.settings.mic_boost == Some(7)
                    && !s.settings_saved
                {
                    panic!("the sink broke");
                }
                let _ = tx.send(event);
            },
            Box::new(SystemTime::now),
            LIMITS,
        )
        .unwrap();
        engine.handle().send(Command::SetMicBoost(7));
        eventually("the worker to end", || !engine.is_running());

        let saved = config::load_from(&dir.join("config.toml")).unwrap();
        assert_eq!(saved.mic_boost, Some(7));
        let events: Vec<Event> = rx.try_iter().collect();
        let Some(Event::State(last)) = events.last() else {
            panic!("the last event should be a state: {events:?}");
        };
        assert_eq!(last.phase, Phase::Stopped);
        assert!(last.settings_saved);
        assert_eq!(last.error.as_ref().map(|e| e.kind), Some(ErrorKind::Fatal));
        drop(NamedLock::acquire(&name).unwrap());
    }

    /// Limits short enough for a test to wait out.
    const QUICK: Limits = Limits {
        stall: Duration::from_millis(150),
        stop_wait: Duration::from_secs(10),
    };

    /// A running engine whose first call at the place that `block_at` picks blocks until the
    /// sender is used.
    fn stuck_engine(
        tag: &str,
        config: &str,
        block_at: impl FnOnce(&TestBackend) -> &Mutex<Box<dyn Fn() + Send>>,
    ) -> (Running, mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let first = AtomicBool::new(true);
        let running = Running::build(
            tag,
            config,
            true,
            |backend| {
                *block_at(backend).lock().unwrap() = Box::new(move || {
                    if first.swap(false, Ordering::SeqCst) {
                        let _ = reached_tx.send(());
                        let _ = release_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(30));
                    }
                });
            },
            Box::new(SystemTime::now),
            QUICK,
        );
        reached_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the engine reaches the call");
        (running, release_tx)
    }

    fn stuck_opening(tag: &str) -> (Running, mpsc::Sender<()>) {
        stuck_engine(tag, "mode = \"balanced\"\n", |backend| &backend.on_open)
    }

    fn unresponsive(state: &Snapshot) -> bool {
        state
            .error
            .as_ref()
            .is_some_and(|e| e.kind == ErrorKind::Unresponsive)
    }

    #[test]
    fn a_call_into_the_dongle_that_never_returns_is_reported_and_does_not_hold_up_stop() {
        let (mut running, release) = stuck_opening("stall-stop");
        let state = running.wait_for("the report", unresponsive);
        let message = state.error.unwrap().message;
        assert!(message.contains("does not answer"), "{message}");
        assert!(message.contains("replug"), "{message}");
        assert_eq!(running.errors.borrow().len(), 1, "reported once");
        assert!(running.engine.as_ref().unwrap().is_running());

        // stopping does not wait for the call: the stop wait is 10 s here
        let started = Instant::now();
        running.stop();
        assert!(started.elapsed() < Duration::from_secs(5));

        // the call ends later; the worker, which was asked to stop, lets go of the lock
        release.send(()).unwrap();
        let name = lock_name("stall-stop");
        eventually("the lock", || NamedLock::acquire(&name).is_ok());
    }

    #[test]
    fn the_engine_goes_on_when_the_stuck_call_returns() {
        let (running, release) = stuck_opening("stall-recover");
        running.wait_for("the report", unresponsive);
        release.send(()).unwrap();

        // the report is taken back: the next state has no such error, and the dongle is in order
        let state = running.wait_for("the recovery", |s| {
            s.phase == Phase::Connected && s.in_sync() && s.error.is_none()
        });
        assert!(state.settings_saved);
        assert!(running.engine.as_ref().unwrap().is_running());
    }

    #[test]
    fn the_report_of_a_stuck_call_is_taken_back_even_when_nothing_else_changed() {
        let (reached_tx, reached_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let armed = Arc::new(AtomicBool::new(false));
        let running = Running::build(
            "stall-quiet",
            "mode = \"balanced\"\n",
            true,
            {
                let armed = Arc::clone(&armed);
                |backend| {
                    *backend.on_open.lock().unwrap() = Box::new(move || {
                        if armed.swap(false, Ordering::SeqCst) {
                            let _ = reached_tx.send(());
                            let _ = release_rx
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_secs(30));
                        }
                    });
                }
            },
            Box::new(SystemTime::now),
            QUICK,
        );
        running.in_sync();

        // the dongle is in order and nothing is going on when a read gets stuck
        armed.store(true, Ordering::SeqCst);
        running.handle().send(Command::Refresh);
        reached_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the engine reads the dongle");
        running.wait_for("the report", unresponsive);
        release_tx.send(()).unwrap();

        // the dongle looks as it did before the stall, so only the report tells a new state
        let state = running.wait_for("the report taken back", |s| s.error.is_none());
        assert!(state.in_sync());
        assert_eq!(state.phase, Phase::Connected);
    }

    #[test]
    fn a_stuck_listing_of_the_devices_and_a_stuck_silent_stream_are_reported_too() {
        let (running, release) = stuck_engine("stall-list", "mode = \"balanced\"\n", |backend| {
            &backend.on_present
        });
        // the very first listing of the devices: no state has been published yet
        running.wait_for_error(ErrorKind::Unresponsive);
        release.send(()).unwrap();
        running.in_sync();

        // a mode change needs the stream
        let (running, release) = stuck_engine("stall-stream", "mode = \"quiet\"\n", |backend| {
            &backend.on_stream
        });
        running.wait_for("the report", unresponsive);
        release.send(()).unwrap();
        running.in_sync();
    }

    #[test]
    fn the_public_start_takes_the_device_lock_whatever_the_backend() {
        let dir = TempDir::new("engine-public");
        let log_path = dir.join("log").join("cxtune.log");
        // Either this test holds the lock or a running cxtune does; both make it busy. The
        // backend is the fake in any case.
        let _held = NamedLock::device();
        let backend = TestBackend::new(FakeDevice::stock(), true);
        let result = Engine::start_with(
            dir.join("config.toml"),
            Some(log_path.clone()),
            move || Ok(backend),
            |_| {},
        );
        assert!(matches!(result, Err(Error::Busy(_))));
        // a refused start leaves nothing behind
        assert!(!log_path.exists());
        assert!(!dir.join("log").exists());
    }

    /// The longest the worker loop waits between two passes.
    const POLL_BOUND: Duration = Duration::from_secs(1);
}
