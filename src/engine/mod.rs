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
//!
//! The decisions live in [`supervisor`]; this module is the thread around it and the two
//! environments it can run in: the real dongle ([`HidBackend`]) and the tests' fake.

mod supervisor;
mod types;

#[cfg(test)]
mod testing;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use hidapi::HidApi;
use supervisor::Supervisor;
pub use types::{Command, ErrorKind, Event, Failure, Phase, Readback, Snapshot};

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

/// A running engine. Dropping it stops it: the worker finishes what it is doing, saves the
/// settings and releases the device lock.
pub struct Engine {
    handle: Handle,
    worker: Option<JoinHandle<()>>,
}

impl Engine {
    /// Starts the engine on the real dongle, with the settings from `%APPDATA%\cxtune` and a
    /// diagnostic log in `%LOCALAPPDATA%\cxtune`.
    ///
    /// Fails with [`Error::Busy`] if another cxtune process controls the dongle, and with
    /// [`Error::Config`] if the settings file cannot be read. Nothing has touched the dongle
    /// by then.
    pub fn start(sink: impl FnMut(Event) + Send + 'static) -> Result<Engine> {
        let lock = NamedLock::device()?;
        let log = log::default_path().map(|path| Log::open(path, log::DEFAULT_LIMIT));
        Engine::start_with(lock, config::path()?, log, HidBackend::new, sink)
    }

    /// Starts the engine on any [`Backend`]. The worker holds `lock` until it ends. The backend
    /// is made on the worker thread, so it need not be `Send`; a failure to make it is returned
    /// here.
    pub fn start_with<B: Backend + 'static>(
        lock: NamedLock,
        config_path: PathBuf,
        log: Option<Log>,
        make_backend: impl FnOnce() -> Result<B> + Send + 'static,
        sink: impl FnMut(Event) + Send + 'static,
    ) -> Result<Engine> {
        Engine::launch(
            lock,
            config_path,
            log,
            make_backend,
            sink,
            Box::new(SystemTime::now),
        )
    }

    /// [`Engine::start_with`] with the wall clock that notices sleep made replaceable.
    fn launch<B: Backend + 'static>(
        lock: NamedLock,
        config_path: PathBuf,
        log: Option<Log>,
        make_backend: impl FnOnce() -> Result<B> + Send + 'static,
        sink: impl FnMut(Event) + Send + 'static,
        wall_clock: Box<dyn Fn() -> SystemTime + Send>,
    ) -> Result<Engine> {
        let cfg = config::load_from(&config_path)?;
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("cxtune-engine".into())
            .spawn(move || {
                let _lock = lock;
                let backend = match make_backend() {
                    Ok(backend) => backend,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let mut log = log;
                let supervisor = Supervisor::new(
                    backend,
                    cfg,
                    config_path,
                    Box::new(sink),
                    Box::new(move |message| {
                        if let Some(log) = &mut log {
                            log.line(message);
                        }
                    }),
                    Box::new(Instant::now),
                );
                let _ = ready_tx.send(Ok(()));
                run(supervisor, &rx, &*wall_clock);
            })
            .map_err(|e| Error::Device(format!("could not start the engine thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Engine {
                handle: Handle { tx },
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = worker.join();
                Err(Error::Device("the engine thread ended unexpectedly".into()))
            }
        }
    }

    /// A sender for commands that can be handed to other threads.
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Stops the engine and waits for the worker to finish what it is doing.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.handle.tx.send(Message::Stop);
            let _ = worker.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
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
    mut supervisor: Supervisor<B>,
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

    fn lock_name(tag: &str) -> String {
        format!("Local\\cxtune-test-engine-{tag}-{}", std::process::id())
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cxtune-engine-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An engine on the fake dongle, with its events collected on a channel.
    struct Running {
        engine: Option<Engine>,
        events: Receiver<Event>,
        /// States taken from the channel so far.
        consumed: std::cell::Cell<usize>,
        dev: FakeDevice,
        dir: PathBuf,
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
            let dir = scratch(tag);
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
            )
            .unwrap_or_else(|e| panic!("{e}"));
            Running {
                engine: Some(engine),
                events,
                consumed: std::cell::Cell::new(0),
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
                    Ok(_) => {}
                    Err(_) => panic!("timed out waiting for {what}; states seen: {seen:#?}"),
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

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
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
        let dir = scratch("holds");
        let backend = TestBackend::new(FakeDevice::stock(), false);
        let engine = Engine::start_with(
            NamedLock::acquire(&name).unwrap(),
            dir.join("config.toml"),
            None,
            move || Ok(backend),
            |_| {},
        )
        .unwrap();
        assert!(matches!(NamedLock::acquire(&name), Err(Error::Busy(_))));
        engine.stop();
        drop(NamedLock::acquire(&name).unwrap());

        // dropping an engine releases the lock in the same way
        let backend = TestBackend::new(FakeDevice::stock(), false);
        let engine = Engine::start_with(
            NamedLock::acquire(&name).unwrap(),
            dir.join("config.toml"),
            None,
            move || Ok(backend),
            |_| {},
        )
        .unwrap();
        assert!(matches!(NamedLock::acquire(&name), Err(Error::Busy(_))));
        drop(engine);
        drop(NamedLock::acquire(&name).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_start_that_fails_releases_the_lock() {
        let name = lock_name("fails");
        let dir = scratch("fails");

        // a settings file from an earlier version is rejected before anything runs
        std::fs::write(dir.join("config.toml"), "profile = \"idle\"\n").unwrap();
        let backend = TestBackend::new(FakeDevice::stock(), true);
        let result = Engine::start_with(
            NamedLock::acquire(&name).unwrap(),
            dir.join("config.toml"),
            None,
            move || Ok(backend),
            |_| {},
        );
        let Err(Error::Config(message)) = result else {
            panic!("the start should fail on the settings file")
        };
        assert!(message.contains("profile"), "{message}");
        drop(NamedLock::acquire(&name).unwrap());

        // so is a backend that cannot be made
        std::fs::remove_file(dir.join("config.toml")).unwrap();
        let result = Engine::start_with(
            NamedLock::acquire(&name).unwrap(),
            dir.join("config.toml"),
            None,
            || -> Result<TestBackend> { Err(Error::Device("no HID".into())) },
            |_| {},
        );
        assert!(matches!(result, Err(Error::Device(_))));
        drop(NamedLock::acquire(&name).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The longest the worker loop waits between two passes.
    const POLL_BOUND: Duration = Duration::from_secs(1);
}
