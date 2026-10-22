//! The behaviour behind the window: the user's choices go to the engine as commands, and what
//! the engine reports is shown. The thread of the window never touches the dongle: the engine
//! is started and stopped on threads of their own.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use cxtune::config;
use cxtune::engine::{self, Command, Engine, ErrorKind, Event, Handle, Snapshot};
use cxtune::mode::Mode;
use cxtune::volume::Side;
use slint::{CloseRequestResponse, ComponentHandle, Timer, TimerMode, Weak};

use crate::controls::{self, Controls};
use crate::lang::{self, Lang};
use crate::platform;
use crate::sound::{Levels, Sound};
use crate::status::{self, Problem};
use crate::throttle::Throttle;
use crate::ui::{MainWindow, Model, Tray};
use crate::{autostart, instance};

/// The index of the About tab of the window.
const ABOUT_TAB: i32 = 4;
/// A slider sends at most one command per this long, and the last value always.
const PACE: Duration = Duration::from_millis(100);
/// After the user moves a volume slider, what the thread reads is ignored for this long, so that
/// the slider does not jump back to a value from before the move.
const VOLUME_HOLD: Duration = Duration::from_millis(1200);
/// How long Quit waits for an engine that is still being started, which it stops first, before
/// it gives up on a start that does not return.
const START_GRACE: Duration = Duration::from_secs(3);

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the application state. Only the thread of the event loop has any.
pub fn with_app(f: impl FnOnce(&mut App)) {
    APP.with(|app| {
        if let Some(app) = app.borrow_mut().as_mut() {
            f(app);
        }
    });
}

/// The controls the user has used since the engine started. Those keep what the user chose; the
/// others follow the settings file and the dongle.
#[derive(Default)]
struct Touched {
    mode: bool,
    settle: bool,
    boost: bool,
    noise_reduction: bool,
    agc: bool,
    sidetone: bool,
    level: bool,
}

#[derive(Clone, Copy)]
enum Slot {
    Settle,
    Boost,
    Level,
}

const SLOTS: [Slot; 3] = [Slot::Settle, Slot::Boost, Slot::Level];

struct Paced {
    throttle: Throttle<Command>,
    timer: Timer,
}

pub struct App {
    window: Weak<MainWindow>,
    tray: Weak<Tray>,
    engine: Option<Engine>,
    handle: Option<Handle>,
    /// Tells the events and the result of an engine that was replaced from those of the current
    /// one.
    generation: usize,
    /// Whether an engine is being started; its result comes back to `on_started`.
    starting: bool,
    snapshot: Option<Snapshot>,
    start_problem: Option<Problem>,
    /// Whether the engine reported something unusual that Re-apply has not acknowledged yet.
    warned: bool,
    controls: Controls,
    touched: Touched,
    paced: [Paced; 3],
    sound: Sound,
    /// Until when the volume of each side, playback and capture, is the user's own move.
    volume_hold: [Option<Instant>; 2],
    quitting: bool,
}

/// Builds the application state and connects the window and the tray icon to it.
pub fn install(
    window: &MainWindow,
    tray: &Tray,
    lang: Lang,
    guard: &instance::Guard,
    visible: bool,
) {
    let model = window.global::<Model>();
    model.set_version(env!("CARGO_PKG_VERSION").into());
    model.set_settle_min(f32::from(controls::SETTLE_MIN_MS));
    model.set_settle_max(f32::from(controls::SETTLE_MAX_MS));
    model.set_settle_step(f32::from(cxtune::mode::SETTLE_STEP_MS));
    model.set_boost_max(f32::from(controls::BOOST_MAX));
    model.set_level_positions(controls::LEVEL_POSITIONS);
    model.set_autostart(autostart::is_enabled().unwrap_or(false));

    let paced = || Paced {
        throttle: Throttle::new(PACE),
        timer: Timer::default(),
    };
    let mut app = App {
        window: window.as_weak(),
        tray: tray.as_weak(),
        engine: None,
        handle: None,
        generation: 0,
        starting: false,
        snapshot: None,
        start_problem: None,
        warned: false,
        controls: Controls::VENDOR,
        touched: Touched::default(),
        paced: [paced(), paced(), paced()],
        sound: Sound::start(|levels| {
            let _ = slint::invoke_from_event_loop(move || with_app(|app| app.on_sound(levels)));
        }),
        volume_hold: [None; 2],
        quitting: false,
    };
    app.sound.watch(visible);
    app.apply_language(lang);
    APP.with(|slot| *slot.borrow_mut() = Some(app));

    connect(window, tray);
    guard.listen(|| {
        let _ = slint::invoke_from_event_loop(|| with_app(App::show_window));
    });
}

/// Starts the engine. Called once the window and the tray icon are shown, so that a start that
/// is slow does not keep them from appearing.
pub fn start() {
    with_app(|app| app.start_engine(None));
}

fn connect(window: &MainWindow, tray: &Tray) {
    let model = window.global::<Model>();
    model.on_mode_selected(|index| with_app(|app| app.mode_selected(index)));
    model.on_settle_changed(|ms| with_app(|app| app.settle_changed(ms)));
    model.on_boost_changed(|step| with_app(|app| app.boost_changed(step)));
    model.on_noise_reduction_toggled(|on| with_app(|app| app.noise_reduction_toggled(on)));
    model.on_agc_toggled(|on| with_app(|app| app.agc_toggled(on)));
    model.on_sidetone_toggled(|on| with_app(|app| app.sidetone_toggled(on)));
    model.on_level_changed(|position| with_app(|app| app.level_changed(position)));
    model.on_slider_released(|| with_app(App::flush_all));
    model.on_autostart_toggled(|on| with_app(|app| app.autostart_toggled(on)));
    model.on_language_selected(|index| with_app(|app| app.language_selected(index)));
    model.on_apply_recommended(|| with_app(App::apply_recommended));
    model.on_volume_changed(|side, percent| {
        with_app(|app| app.volume_changed(side_of(side), percent));
    });
    model.on_mute_toggled(|side, muted| with_app(|app| app.mute_toggled(side_of(side), muted)));
    model.on_reapply(|| with_app(App::reapply));
    model.on_open_log_folder(open_log_folder);

    window.window().on_close_requested(|| {
        with_app(|app| app.sound.watch(false));
        CloseRequestResponse::HideWindow
    });

    tray.on_show_requested(|| with_app(App::show_window));
    tray.on_mode_requested(|index| with_app(|app| app.mode_selected(index)));
    tray.on_reapply_requested(|| with_app(App::reapply));
    tray.on_about_requested(|| with_app(App::show_about));
    tray.on_icon_clicked(|| with_app(App::show_window));
    tray.on_quit_requested(|| with_app(App::quit));
}

/// The side that the interface calls 0 (the speakers) or 1 (the microphone).
fn side_of(index: i32) -> Side {
    if index == 0 {
        Side::Playback
    } else {
        Side::Capture
    }
}

/// Appends a line to the log file, for what the engine does not log itself.
fn log(line: &str) {
    if let Some(path) = cxtune::log::default_path() {
        cxtune::log::Log::open(path, cxtune::log::DEFAULT_LIMIT).line(line);
    }
}

fn open_log_folder() {
    if let Some(folder) = cxtune::log::default_path()
        .as_deref()
        .and_then(|p| p.parent())
    {
        platform::open_folder(folder);
    }
}

/// Drops `engine` on a thread of its own: stopping one can take seconds.
fn release(engine: Option<Engine>) {
    if engine.is_some() {
        std::thread::spawn(move || drop(engine));
    }
}

/// Stops `engine` on a thread of its own, because finishing a mode change takes seconds, and
/// ends the event loop when it has let go of the dongle.
fn stop_and_quit(engine: Option<Engine>) {
    std::thread::spawn(move || {
        drop(engine);
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    });
}

impl App {
    /// Starts an engine on a thread of its own, because making one enumerates the HID devices
    /// and a device that is slow to answer would freeze the window. The `previous` engine is
    /// stopped there first, so that the lock it holds is free, or known to be stuck, by then.
    /// The result comes back to `on_started`.
    fn start_engine(&mut self, previous: Option<Engine>) {
        self.generation += 1;
        let generation = self.generation;
        self.starting = true;
        self.handle = None;
        self.snapshot = None;
        self.start_problem = None;
        self.touched = Touched::default();
        let spawned = std::thread::Builder::new()
            .name("cxtune-start".into())
            .spawn(move || {
                drop(previous);
                let started = Engine::start(move |event| {
                    let _ = slint::invoke_from_event_loop(move || {
                        with_app(|app| app.on_event(generation, event));
                    });
                });
                // an engine that cannot be delivered because the loop has ended is dropped here
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|app| app.on_started(generation, started));
                });
            });
        if let Err(error) = spawned {
            self.starting = false;
            log(&format!("could not start the engine thread: {error}"));
            self.start_problem = Some(Problem::StartFailed);
        }
        self.render();
    }

    fn on_started(&mut self, generation: usize, started: cxtune::Result<Engine>) {
        if generation != self.generation {
            release(started.ok());
            return;
        }
        self.starting = false;
        if self.quitting {
            stop_and_quit(started.ok());
            return;
        }
        match started {
            Ok(engine) => {
                self.handle = Some(engine.handle());
                self.engine = Some(engine);
            }
            Err(error) => {
                log(&format!("could not start the engine: {error}"));
                self.start_problem =
                    Some(status::start_problem(&error, engine::worker_left_behind()));
            }
        }
        self.render();
    }

    fn on_event(&mut self, generation: usize, event: Event) {
        if generation != self.generation {
            return;
        }
        match event {
            Event::State(snapshot) => self.snapshot = Some(snapshot),
            // the engine logs a warning itself
            Event::Warning(_) => self.warned = true,
            // A problem is part of the state; a rejected value is not. The engine logs its own
            // errors, except those that its watchdog finds and those it rejects.
            Event::Error(failure) => {
                if matches!(failure.kind, ErrorKind::Invalid | ErrorKind::Unresponsive) {
                    log(&format!("error ({}): {}", failure.kind, failure.message));
                }
                self.warned |= failure.kind == ErrorKind::Invalid;
            }
            Event::Test(_) => {}
        }
        self.render();
    }

    fn render(&mut self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let model = window.global::<Model>();
        let status = status::status(self.snapshot.as_ref(), self.start_problem, self.warned);

        let shown = match &self.snapshot {
            Some(snapshot) => Controls::effective(&snapshot.settings, snapshot.device.as_ref()),
            None => Controls::effective(&config::load().unwrap_or_default(), None),
        };
        let touched = &self.touched;
        let controls = &mut self.controls;
        if !touched.mode {
            controls.mode = shown.mode;
            model.set_mode(controls::mode_index(shown.mode));
        }
        if !touched.settle {
            controls.settle_ms = shown.settle_ms;
            model.set_settle_ms(f32::from(shown.settle_ms));
        }
        if !touched.boost {
            controls.boost = shown.boost;
            model.set_boost(f32::from(shown.boost));
        }
        if !touched.noise_reduction {
            controls.noise_reduction = shown.noise_reduction;
            model.set_noise_reduction(shown.noise_reduction);
        }
        if !touched.agc {
            controls.agc = shown.agc;
            model.set_agc(shown.agc);
        }
        if !touched.sidetone {
            controls.sidetone = shown.sidetone;
            model.set_sidetone(shown.sidetone);
        }
        if !touched.level {
            controls.level = shown.level;
            model.set_level_position(controls::level_position(shown.level));
        }
        model.set_level(i32::from(controls.level));

        let running = self.engine.as_ref().is_some_and(Engine::is_running);
        let usable = running && status.controls;
        let controls = self.controls;
        model.set_controls_enabled(usable);
        model.set_mode_recommended(controls.mode == controls::RECOMMENDED_MODE);
        model.set_delay_enabled(usable && controls.mode == Mode::MinHiss);
        let indicators = controls::indicators(controls.mode, controls.settle_ms);
        model.set_hiss(indicators.hiss);
        model.set_pop(indicators.pop);
        model.set_delay(indicators.delay);
        model.set_delay_ms(indicators.delay_ms);
        model.set_boost_db(controls::gain_db(controls.boost));
        model.set_boost_recommended(controls.boost == controls::BOOST_RECOMMENDED);

        model.set_title(status.title);
        model.set_hint(status.hint);
        model.set_tone(status.tone);
        model.set_details(status.details || self.warned);
        model.set_reapply_enabled(status.reapply);

        if let Some(tray) = self.tray.upgrade() {
            tray.set_status_title(status.title);
            tray.set_mode(controls::mode_index(controls.mode));
            tray.set_controls_enabled(usable);
            tray.set_reapply_enabled(status.reapply);
        }
    }

    fn send(&self, command: Command) {
        if let Some(handle) = &self.handle {
            handle.send(command);
        }
    }

    /// Sends a command that a slider produces many of: at once if the last one was a while ago,
    /// else the newest of them once the pause is over.
    fn send_paced(&mut self, slot: Slot, command: Command) {
        let now = Instant::now();
        let paced = &mut self.paced[slot as usize];
        match paced.throttle.offer(now, command) {
            Some(command) => self.send(command),
            None => {
                let wait = paced.throttle.due_in(now).unwrap_or_default();
                paced.timer.start(TimerMode::SingleShot, wait, move || {
                    with_app(|app| app.flush(slot));
                });
            }
        }
    }

    fn flush(&mut self, slot: Slot) {
        if let Some(command) = self.paced[slot as usize].throttle.flush(Instant::now()) {
            self.send(command);
        }
    }

    fn flush_all(&mut self) {
        for slot in SLOTS {
            self.flush(slot);
        }
    }

    fn mode_selected(&mut self, index: i32) {
        let Some(mode) = controls::mode_at(index) else {
            return;
        };
        self.touched.mode = true;
        self.controls.mode = mode;
        if let Some(window) = self.window.upgrade() {
            window.global::<Model>().set_mode(index);
        }
        self.send(Command::SetMode(mode));
        self.render();
    }

    fn apply_recommended(&mut self) {
        let next = self.controls.recommended();
        self.touched.mode = true;
        self.touched.boost = true;
        self.touched.noise_reduction = true;
        self.touched.agc = true;
        self.touched.sidetone = true;
        self.controls = next;
        if let Some(window) = self.window.upgrade() {
            let model = window.global::<Model>();
            model.set_mode(controls::mode_index(next.mode));
            model.set_boost(f32::from(next.boost));
            model.set_noise_reduction(next.noise_reduction);
            model.set_agc(next.agc);
            model.set_sidetone(next.sidetone);
        }
        self.flush_all();
        self.send(Command::SetMode(next.mode));
        self.send(Command::SetMicBoost(next.boost));
        self.send(Command::SetNoiseReduction(next.noise_reduction));
        self.send(Command::SetAgc(next.agc));
        self.send(Command::SetSidetone(next.sidetone));
        self.render();
    }

    fn volume_changed(&mut self, side: Side, percent: i32) {
        let Ok(percent) = u8::try_from(percent) else {
            return;
        };
        self.volume_hold[side as usize] = Some(Instant::now() + VOLUME_HOLD);
        self.sound.set(side, percent);
    }

    fn mute_toggled(&mut self, side: Side, muted: bool) {
        self.volume_hold[side as usize] = Some(Instant::now() + VOLUME_HOLD);
        self.sound.mute(side, muted);
    }

    /// Shows the volume that the thread has read, except where the user is moving a slider.
    fn on_sound(&mut self, levels: Levels) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let model = window.global::<Model>();
        let now = Instant::now();
        let free = |side: Side| self.volume_hold[side as usize].is_none_or(|until| now >= until);
        if free(Side::Playback) {
            let level = levels.of(Side::Playback);
            model.set_speaker_present(level.is_some());
            if let Some(level) = level {
                model.set_speaker_volume(f32::from(level.percent));
                model.set_speaker_muted(level.muted);
            }
        }
        if free(Side::Capture) {
            let level = levels.of(Side::Capture);
            model.set_microphone_present(level.is_some());
            if let Some(level) = level {
                model.set_microphone_volume(f32::from(level.percent));
                model.set_microphone_muted(level.muted);
            }
        }
    }

    fn settle_changed(&mut self, ms: i32) {
        let Ok(ms) = u16::try_from(ms) else {
            return;
        };
        if ms == self.controls.settle_ms {
            return;
        }
        self.touched.settle = true;
        self.controls.settle_ms = ms;
        self.send_paced(Slot::Settle, Command::SetMinHissMs(ms));
        self.render();
    }

    fn boost_changed(&mut self, step: i32) {
        let Ok(step) = u8::try_from(step) else {
            return;
        };
        if step == self.controls.boost {
            return;
        }
        self.touched.boost = true;
        self.controls.boost = step;
        self.send_paced(Slot::Boost, Command::SetMicBoost(step));
        self.render();
    }

    fn noise_reduction_toggled(&mut self, on: bool) {
        self.touched.noise_reduction = true;
        self.controls.noise_reduction = on;
        self.send(Command::SetNoiseReduction(on));
    }

    fn agc_toggled(&mut self, on: bool) {
        self.touched.agc = true;
        self.controls.agc = on;
        self.send(Command::SetAgc(on));
    }

    fn sidetone_toggled(&mut self, on: bool) {
        self.flush_all();
        let first = on
            .then(|| controls::level_before_sidetone(self.controls.level, self.touched.level))
            .flatten();
        if let Some(level) = first {
            self.touched.level = true;
            self.controls.level = level;
            if let Some(window) = self.window.upgrade() {
                let model = window.global::<Model>();
                model.set_level(i32::from(level));
                model.set_level_position(controls::level_position(level));
            }
            self.send(Command::SetSidetoneLevel(level));
        }
        self.touched.sidetone = true;
        self.controls.sidetone = on;
        self.send(Command::SetSidetone(on));
    }

    fn level_changed(&mut self, position: f32) {
        let level = controls::level_at(position);
        if level == self.controls.level {
            return;
        }
        self.touched.level = true;
        self.controls.level = level;
        if let Some(window) = self.window.upgrade() {
            window.global::<Model>().set_level(i32::from(level));
        }
        self.send_paced(Slot::Level, Command::SetSidetoneLevel(level));
    }

    fn autostart_toggled(&mut self, on: bool) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let model = window.global::<Model>();
        let failed = autostart::set_enabled(on).is_err();
        if failed {
            model.set_autostart(!on);
        }
        model.set_autostart_failed(failed);
    }

    fn language_selected(&mut self, index: i32) {
        let lang = if index == 0 {
            Lang::Turkish
        } else {
            Lang::English
        };
        self.apply_language(lang);
        if let Some(path) = lang::preferences_path() {
            let _ = lang::save(&path, lang);
        }
    }

    pub fn apply_language(&mut self, lang: Lang) {
        let _ = slint::select_bundled_translation(lang.code());
        if let Some(window) = self.window.upgrade() {
            window
                .global::<Model>()
                .set_language(if lang == Lang::Turkish { 0 } else { 1 });
        }
        self.render();
    }

    fn reapply(&mut self) {
        self.warned = false;
        if self.starting {
            return;
        }
        if self.engine.as_ref().is_some_and(Engine::is_running) {
            self.send(Command::Reapply);
            self.render();
        } else {
            let previous = self.engine.take();
            self.start_engine(previous);
        }
    }

    fn show_about(&mut self) {
        if let Some(window) = self.window.upgrade() {
            let model = window.global::<Model>();
            model.set_tab(ABOUT_TAB);
        }
        self.show_window();
    }

    fn show_window(&mut self) {
        if let Some(window) = self.window.upgrade() {
            platform::show_and_raise(window.window());
            self.sound.watch(true);
        }
    }

    /// Hides the window and the icon and stops the engine (see `stop_and_quit`). An engine that
    /// is still being started is stopped when it arrives, and a start that does not return does
    /// not keep the program alive for longer than `START_GRACE`.
    pub fn quit(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        if let Some(window) = self.window.upgrade() {
            let _ = window.hide();
        }
        if let Some(tray) = self.tray.upgrade() {
            let _ = tray.hide();
        }
        if self.starting {
            Timer::single_shot(START_GRACE, || {
                with_app(|app| {
                    if app.starting {
                        let _ = slint::quit_event_loop();
                    }
                });
            });
        } else {
            stop_and_quit(self.engine.take());
        }
    }
}
