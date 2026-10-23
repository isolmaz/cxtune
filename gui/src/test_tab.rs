//! The Test page: what it asks of the engine, and how the engine's answers are shown. The tests
//! themselves, and the rules for which of them may run together, are the engine's (docs/engine.md);
//! the page only follows them.

use cxtune::engine::{
    Command, ErrorKind, Snapshot, TestEvent, TestKind, TestOutcome, TestProgress, TestRequest,
    TestState, TestStep as EngineStep,
};

use slint::ComponentHandle;

use crate::ui::{MainWindow, Measured, Model, TestJob, TestProblem, TestStep};

/// The index of the Test page in the window.
pub const TAB: i32 = 3;
/// How long the measurements run, in seconds.
pub const NOISE_FLOOR_SECONDS: u8 = 10;
pub const RECORD_SECONDS: u8 = 5;
/// Each half of the noise reduction comparison.
pub const COMPARISON_SECONDS: u8 = 10;

/// What the page quotes at rest, for a level that no meter delivered.
const NO_LEVEL_DB: f32 = -60.0;

/// Whether the Test page can reach the dongle: the engine runs (`usable`) and has read the dongle
/// that is connected. A disconnect and a wake leave `Snapshot::device` empty until the dongle has
/// been read again, which is when the engine would refuse what the page asks.
pub fn available(usable: bool, snapshot: Option<&Snapshot>) -> bool {
    usable && snapshot.is_some_and(|snapshot| snapshot.device.is_some())
}

/// Whether the Test page is in view: it is the page of a window that is shown (`shown` is false
/// while the window is hidden in the notification area) and not minimized.
pub fn in_view(window: &MainWindow, shown: bool) -> bool {
    window.global::<Model>().get_tab() == TAB && shown && !window.window().is_minimized()
}

/// What the engine is asked for a test of the page.
pub fn request_of(job: TestJob) -> Option<TestRequest> {
    match job {
        TestJob::None => None,
        TestJob::NoiseFloor => Some(TestRequest::NoiseFloor {
            seconds: NOISE_FLOOR_SECONDS,
        }),
        TestJob::Record => Some(TestRequest::Record {
            seconds: RECORD_SECONDS,
        }),
        TestJob::Play => Some(TestRequest::PlayRecording),
        TestJob::Comparison => Some(TestRequest::NoiseReductionAb {
            seconds: COMPARISON_SECONDS,
        }),
        TestJob::Click => Some(TestRequest::ClickTest),
        TestJob::Listen => Some(TestRequest::NoiseListen),
    }
}

fn job_of(kind: TestKind) -> TestJob {
    match kind {
        TestKind::NoiseFloor => TestJob::NoiseFloor,
        TestKind::Record => TestJob::Record,
        TestKind::PlayRecording => TestJob::Play,
        TestKind::NoiseReductionAb => TestJob::Comparison,
        TestKind::ClickTest => TestJob::Click,
        TestKind::NoiseListen => TestJob::Listen,
    }
}

fn step_of(step: EngineStep) -> TestStep {
    match step {
        EngineStep::WarmingUp => TestStep::WarmingUp,
        EngineStep::Measuring => TestStep::Measuring,
        EngineStep::Recording => TestStep::Recording,
        EngineStep::MeasuringWithNoiseReduction => TestStep::WithNoiseReduction,
        EngineStep::SwitchingNoiseReduction => TestStep::Switching,
        EngineStep::MeasuringWithoutNoiseReduction => TestStep::WithoutNoiseReduction,
        EngineStep::WaitingForIdle => TestStep::Waiting,
        EngineStep::Resting => TestStep::Resting,
        EngineStep::Playing => TestStep::Playing,
    }
}

fn problem_of(kind: ErrorKind) -> TestProblem {
    match kind {
        ErrorKind::Communication => TestProblem::Communication,
        ErrorKind::Audio => TestProblem::Audio,
        _ => TestProblem::Other,
    }
}

fn measured(first: f32, second: f32, third: f32) -> Measured {
    Measured {
        present: true,
        first,
        second,
        third,
    }
}

/// What the Test page shows: the test that runs and its progress, the last result of each test,
/// the last problem, the switch of the meter and its level.
#[derive(Default)]
pub struct TestTab {
    running: Option<TestKind>,
    progress: Option<TestProgress>,
    has_recording: bool,
    /// Whether the page can reach the dongle (`available`).
    available: bool,
    /// The switch of the meter, which only the user turns on. It is turned off wherever the meter
    /// may not run (`state`) and stays off until the user turns it on again.
    meter_on: bool,
    /// What the engine was last asked about its meter.
    asked: bool,
    level: Option<f32>,
    problem: TestProblem,
    /// Whether a test that plays or listens was started or has ended: the caution about it is
    /// hidden from then on.
    sound_started: bool,
    noise_floor: Measured,
    recording: Measured,
    comparison: Measured,
    click: Measured,
    listen: Measured,
}

impl TestTab {
    /// What the engine's state says (which test runs, whether the meter is on, whether there is a
    /// recording) and where the window stands (`available`, `in_view`). The switch of the meter
    /// stays on only where the meter may run: the page is in view, the dongle is there, and no test
    /// that plays a sound or listens runs (the engine closes the microphone for it and refuses to
    /// open it); a measurement keeps the meter as it is. A problem is dropped while the dongle is
    /// not there, so none is left when it returns.
    pub fn state(&mut self, state: &TestState, available: bool, in_view: bool) {
        self.running = state.running;
        self.has_recording = state.has_recording;
        self.available = available;
        if state.running.is_none() {
            self.progress = None;
        }
        if !state.meter {
            self.level = None;
        }
        if !available {
            self.problem = TestProblem::None;
        }
        if !(available && in_view && state.running.is_none_or(TestKind::uses_microphone)) {
            self.switched(false);
        }
    }

    /// The switch of the meter was turned on or off. Without the meter there is no level.
    pub fn switched(&mut self, on: bool) {
        self.meter_on = on;
        if !on {
            self.level = None;
        }
    }

    /// The request that makes the engine's meter follow the switch, once for each change.
    pub fn sync_meter(&mut self) -> Option<Command> {
        if self.meter_on == self.asked {
            return None;
        }
        self.asked = self.meter_on;
        Some(Command::Test(TestRequest::Meter(self.meter_on)))
    }

    /// A new engine has not been asked anything.
    pub fn engine_restarted(&mut self) {
        self.asked = false;
    }

    pub fn event(&mut self, event: &TestEvent) {
        match event {
            TestEvent::Level(level) => {
                if self.meter_on {
                    self.level = Some(level.rms_dbfs);
                }
            }
            TestEvent::Progress(progress) => self.progress = Some(*progress),
            TestEvent::Done(outcome) => {
                self.progress = None;
                self.done(outcome);
            }
            TestEvent::Failed { failure, .. } => {
                self.progress = None;
                self.problem = problem_of(failure.kind);
                // the engine closes the microphone when a test or the meter fails
                self.switched(false);
            }
        }
    }

    fn done(&mut self, outcome: &TestOutcome) {
        match *outcome {
            TestOutcome::NoiseFloor { dbfs } => self.noise_floor = measured(dbfs, 0.0, 0.0),
            TestOutcome::Recorded {
                rms_dbfs,
                peak_dbfs,
                ..
            } => self.recording = measured(rms_dbfs, peak_dbfs, 0.0),
            // what the comparison measured is the change that noise reduction makes
            TestOutcome::NoiseReductionAb {
                with_dbfs,
                without_dbfs,
                reduction_db,
            } => self.comparison = measured(-reduction_db, with_dbfs, without_dbfs),
            TestOutcome::ClickTest { waited_ms } => {
                self.click = measured(waited_ms as f32, 0.0, 0.0);
                self.sound_started = true;
            }
            TestOutcome::NoiseListen { waited_ms } => {
                self.listen = measured(waited_ms as f32, 0.0, 0.0);
                self.sound_started = true;
            }
            TestOutcome::Played => self.sound_started = true,
            TestOutcome::Cancelled(kind) => {
                if let Some(result) = self.result_of(kind) {
                    *result = Measured::default();
                }
            }
        }
    }

    fn result_of(&mut self, kind: TestKind) -> Option<&mut Measured> {
        match kind {
            TestKind::NoiseFloor => Some(&mut self.noise_floor),
            TestKind::Record => Some(&mut self.recording),
            TestKind::NoiseReductionAb => Some(&mut self.comparison),
            TestKind::ClickTest => Some(&mut self.click),
            TestKind::NoiseListen => Some(&mut self.listen),
            TestKind::PlayRecording => None,
        }
    }

    /// The user started `job`: its old result and the last problem are gone. A test that plays a
    /// sound or listens takes the microphone from the meter, which stays off after it; the caution
    /// about its sound has been answered.
    pub fn requested(&mut self, job: TestJob) {
        self.problem = TestProblem::None;
        let result = match job {
            TestJob::NoiseFloor => Some(&mut self.noise_floor),
            TestJob::Record => Some(&mut self.recording),
            TestJob::Comparison => Some(&mut self.comparison),
            TestJob::Click => Some(&mut self.click),
            TestJob::Listen => Some(&mut self.listen),
            TestJob::Play | TestJob::None => None,
        };
        if let Some(result) = result {
            *result = Measured::default();
        }
        if matches!(job, TestJob::Play | TestJob::Click | TestJob::Listen) {
            self.switched(false);
            self.sound_started = true;
        }
    }

    pub fn show(&self, model: &Model) {
        model.set_test_available(self.available);
        model.set_meter_on(self.meter_on);
        model.set_test_job(self.running.map_or(TestJob::None, job_of));
        let progress = self.progress.filter(|p| Some(p.kind) == self.running);
        model.set_test_step(progress.map_or(TestStep::WarmingUp, |p| step_of(p.step)));
        model.set_test_remaining(
            progress
                .and_then(|p| p.remaining_ms)
                .and_then(|ms| i32::try_from(ms).ok())
                .unwrap_or(-1),
        );
        model.set_test_has_recording(self.has_recording);
        model.set_test_problem(self.problem);
        model.set_test_caution(!self.sound_started);
        model.set_noise_floor_result(self.noise_floor.clone());
        model.set_recording_result(self.recording.clone());
        model.set_comparison_result(self.comparison.clone());
        model.set_click_result(self.click.clone());
        model.set_listen_result(self.listen.clone());
        self.show_level(model);
    }

    /// Only the level of the meter, which changes twenty times a second.
    pub fn show_level(&self, model: &Model) {
        model.set_meters_active(self.level.is_some());
        model.set_mic_db(self.level.unwrap_or(NO_LEVEL_DB));
    }
}

#[cfg(test)]
mod tests {
    use cxtune::capture::Level;
    use cxtune::config::Config;
    use cxtune::engine::{Failure, Phase, Readback};
    use cxtune::mode::Mode;

    use super::*;

    fn failure(kind: ErrorKind) -> Failure {
        Failure {
            kind,
            message: "x".into(),
        }
    }

    fn window_on(tab: i32) -> MainWindow {
        i_slint_backend_testing::init_no_event_loop();
        let window = MainWindow::new().unwrap();
        window.global::<Model>().set_tab(tab);
        window
    }

    fn readback() -> Readback {
        Readback {
            firmware_supported: true,
            mode: Some(Mode::Balanced),
            min_hiss_ms: None,
            balanced_decay_ms: None,
            mic_boost: Some(4),
            noise_reduction: true,
            agc: false,
            sidetone: false,
            sidetone_level: 947,
            surround: false,
            surround_level: Some(4),
        }
    }

    fn snapshot(phase: Phase, device: Option<Readback>) -> Snapshot {
        Snapshot {
            phase,
            settings: Config::default(),
            settings_saved: true,
            save_error: None,
            device,
            error: None,
            test: TestState::default(),
        }
    }

    fn idle() -> TestState {
        TestState::default()
    }

    fn running(kind: TestKind) -> TestState {
        TestState {
            running: Some(kind),
            ..TestState::default()
        }
    }

    fn meter(on: bool) -> Option<Command> {
        Some(Command::Test(TestRequest::Meter(on)))
    }

    /// What the application does when something changed: the state of the engine and the window go
    /// into the page, and the request for the meter, if there is one, goes to the engine.
    fn render(
        tab: &mut TestTab,
        test: &TestState,
        available: bool,
        in_view: bool,
    ) -> Option<Command> {
        tab.state(test, available, in_view);
        tab.sync_meter()
    }

    #[test]
    fn the_page_is_in_view_only_as_the_page_of_a_shown_window_that_is_not_minimized() {
        let window = window_on(TAB);
        assert!(in_view(&window, true));
        assert!(!in_view(&window, false), "hidden in the notification area");
        for tab in (0..=TAB + 2).filter(|tab| *tab != TAB) {
            window.global::<Model>().set_tab(tab);
            assert!(!in_view(&window, true), "page {tab}");
        }
        window.global::<Model>().set_tab(TAB);
        window.window().set_minimized(true);
        assert!(!in_view(&window, true), "minimized");
        window.window().set_minimized(false);
        assert!(in_view(&window, true));
    }

    #[test]
    fn the_dongle_is_there_once_the_engine_has_read_the_dongle_that_is_connected() {
        let there = |usable, snapshot: Option<Snapshot>| available(usable, snapshot.as_ref());
        for phase in [Phase::Connected, Phase::Applying] {
            assert!(
                there(true, Some(snapshot(phase, Some(readback())))),
                "{phase:?}"
            );
        }
        // a disconnect and a wake forget what the dongle held until it is read again
        for phase in [Phase::Disconnected, Phase::Waiting, Phase::Connected] {
            assert!(
                !there(true, Some(snapshot(phase, None))),
                "{phase:?} unread"
            );
        }
        let connected = snapshot(Phase::Connected, Some(readback()));
        assert!(
            !there(false, Some(connected)),
            "an engine that does not run"
        );
        assert!(!there(true, None), "an engine that has not reported yet");
    }

    #[test]
    fn entering_the_page_does_not_start_the_meter_and_the_switch_does() {
        let mut tab = TestTab::default();
        assert_eq!(render(&mut tab, &idle(), true, false), None);
        assert_eq!(render(&mut tab, &idle(), true, true), None, "page entered");
        assert!(!tab.meter_on);

        tab.switched(true);
        assert_eq!(render(&mut tab, &idle(), true, true), meter(true));
        assert_eq!(
            render(&mut tab, &idle(), true, true),
            None,
            "once for a change"
        );
        tab.switched(false);
        assert_eq!(render(&mut tab, &idle(), true, true), meter(false));
        assert_eq!(render(&mut tab, &idle(), true, true), None);
    }

    #[test]
    fn the_switch_goes_off_where_the_meter_may_not_run_and_stays_off() {
        let causes = [
            (
                "the page was left, or the window hidden or minimized",
                idle(),
                true,
                false,
            ),
            ("the dongle was lost", idle(), false, true),
            ("a click test", running(TestKind::ClickTest), true, true),
            (
                "noise listening",
                running(TestKind::NoiseListen),
                true,
                true,
            ),
            ("playback", running(TestKind::PlayRecording), true, true),
        ];
        for (cause, test, available, in_view) in causes {
            let mut tab = TestTab::default();
            tab.switched(true);
            assert_eq!(
                render(&mut tab, &idle(), true, true),
                meter(true),
                "{cause}"
            );

            assert_eq!(
                render(&mut tab, &test, available, in_view),
                meter(false),
                "{cause}"
            );
            assert!(!tab.meter_on, "{cause}");

            // what caused it is over, and the microphone is not opened again by itself
            assert_eq!(render(&mut tab, &idle(), true, true), None, "after {cause}");
            assert!(!tab.meter_on, "after {cause}");

            tab.switched(true);
            assert_eq!(
                render(&mut tab, &idle(), true, true),
                meter(true),
                "{cause}"
            );
        }
    }

    #[test]
    fn a_measurement_keeps_the_meter_as_it_is() {
        for kind in [
            TestKind::NoiseFloor,
            TestKind::Record,
            TestKind::NoiseReductionAb,
        ] {
            let mut on = TestTab::default();
            on.switched(true);
            assert_eq!(render(&mut on, &idle(), true, true), meter(true));
            assert_eq!(
                render(&mut on, &running(kind), true, true),
                None,
                "{kind:?}"
            );
            assert_eq!(render(&mut on, &idle(), true, true), None, "{kind:?} ended");
            assert!(on.meter_on, "{kind:?}");

            let mut off = TestTab::default();
            assert_eq!(render(&mut off, &running(kind), true, true), None);
            assert_eq!(render(&mut off, &idle(), true, true), None);
            assert!(!off.meter_on, "{kind:?}");
        }
    }

    #[test]
    fn starting_a_test_that_plays_or_listens_turns_the_switch_off_at_once() {
        for job in [TestJob::Play, TestJob::Click, TestJob::Listen] {
            let mut tab = TestTab::default();
            tab.switched(true);
            assert_eq!(render(&mut tab, &idle(), true, true), meter(true));
            tab.requested(job);
            assert!(!tab.meter_on, "{job:?}");
            assert_eq!(tab.sync_meter(), meter(false), "{job:?}");
        }
        for job in [TestJob::NoiseFloor, TestJob::Record, TestJob::Comparison] {
            let mut tab = TestTab::default();
            tab.switched(true);
            assert_eq!(render(&mut tab, &idle(), true, true), meter(true));
            tab.requested(job);
            assert!(tab.meter_on, "{job:?}");
            assert_eq!(tab.sync_meter(), None, "{job:?}");
        }
    }

    #[test]
    fn a_failure_ends_the_meter_and_the_engine_is_asked_again_when_it_is_turned_on() {
        let mut tab = TestTab::default();
        tab.switched(true);
        assert_eq!(render(&mut tab, &idle(), true, true), meter(true));
        tab.event(&TestEvent::Failed {
            kind: None,
            failure: failure(ErrorKind::Audio),
        });
        assert!(!tab.meter_on);
        assert_eq!(tab.problem, TestProblem::Audio);
        assert_eq!(render(&mut tab, &idle(), true, true), meter(false));

        tab.switched(true);
        assert_eq!(render(&mut tab, &idle(), true, true), meter(true));
    }

    #[test]
    fn losing_the_dongle_disables_the_page_and_closes_the_meter_and_nothing_is_stale_after_it() {
        let window = window_on(TAB);
        let model = window.global::<Model>();
        let mut tab = TestTab::default();
        let connected = snapshot(Phase::Connected, Some(readback()));
        let there = |snapshot: &Snapshot| available(true, Some(snapshot));
        let metering = TestState {
            meter: true,
            ..idle()
        };

        tab.switched(true);
        assert_eq!(
            render(&mut tab, &metering, there(&connected), true),
            meter(true)
        );
        tab.event(&TestEvent::Level(Level {
            rms_dbfs: -52.0,
            peak_dbfs: -40.0,
        }));
        tab.show(&model);
        assert!(model.get_test_available());
        assert!(model.get_meter_on());
        assert!(model.get_meters_active());

        // unplugged: the engine closes the microphone and says so, then reports the state
        tab.event(&TestEvent::Failed {
            kind: None,
            failure: failure(ErrorKind::Communication),
        });
        let gone = snapshot(Phase::Disconnected, None);
        assert_eq!(render(&mut tab, &idle(), there(&gone), true), meter(false));
        tab.show(&model);
        assert!(!model.get_test_available());
        assert!(!model.get_meter_on());
        assert!(!model.get_meters_active());
        assert_eq!(model.get_test_problem(), TestProblem::None);

        // a failure that is reported after the dongle was lost leaves no warning either
        tab.event(&TestEvent::Failed {
            kind: None,
            failure: failure(ErrorKind::Communication),
        });
        assert_eq!(render(&mut tab, &idle(), there(&gone), true), None);
        tab.show(&model);
        assert_eq!(model.get_test_problem(), TestProblem::None);

        // plugged in, or woken: the dongle is not read yet
        let waiting = snapshot(Phase::Waiting, None);
        assert_eq!(render(&mut tab, &idle(), there(&waiting), true), None);
        tab.show(&model);
        assert!(!model.get_test_available());

        // read: the page can be used again, with the switch off and no warning
        assert_eq!(render(&mut tab, &idle(), there(&connected), true), None);
        tab.show(&model);
        assert!(model.get_test_available());
        assert!(!model.get_meter_on());
        assert!(!model.get_meters_active());
        assert_eq!(model.get_test_problem(), TestProblem::None);

        // and the engine is asked for the meter again when the user turns it on
        tab.switched(true);
        assert_eq!(
            render(&mut tab, &idle(), there(&connected), true),
            meter(true)
        );
    }

    #[test]
    fn every_test_of_the_page_makes_its_own_request() {
        assert_eq!(request_of(TestJob::None), None);
        assert_eq!(
            request_of(TestJob::NoiseFloor),
            Some(TestRequest::NoiseFloor { seconds: 10 })
        );
        assert_eq!(
            request_of(TestJob::Record),
            Some(TestRequest::Record { seconds: 5 })
        );
        assert_eq!(request_of(TestJob::Play), Some(TestRequest::PlayRecording));
        assert_eq!(
            request_of(TestJob::Comparison),
            Some(TestRequest::NoiseReductionAb { seconds: 10 })
        );
        assert_eq!(request_of(TestJob::Click), Some(TestRequest::ClickTest));
        assert_eq!(request_of(TestJob::Listen), Some(TestRequest::NoiseListen));
        // the durations are inside what the engine accepts
        const { assert!(NOISE_FLOOR_SECONDS <= TestRequest::MAX_NOISE_FLOOR_SECONDS) };
        const { assert!(RECORD_SECONDS <= TestRequest::MAX_RECORD_SECONDS) };
        const { assert!(COMPARISON_SECONDS <= TestRequest::MAX_AB_SECONDS) };
    }

    #[test]
    fn what_a_test_measured_is_kept_in_the_order_the_page_shows_it() {
        let mut tab = TestTab::default();
        tab.event(&TestEvent::Done(TestOutcome::NoiseFloor { dbfs: -63.9 }));
        assert_eq!(tab.noise_floor, measured(-63.9, 0.0, 0.0));
        tab.event(&TestEvent::Done(TestOutcome::Recorded {
            seconds: 5.0,
            rms_dbfs: -41.2,
            peak_dbfs: -18.0,
        }));
        assert_eq!(tab.recording, measured(-41.2, -18.0, 0.0));
        // the change is the level with noise reduction on minus the level with it off
        tab.event(&TestEvent::Done(TestOutcome::NoiseReductionAb {
            with_dbfs: -63.9,
            without_dbfs: -52.0,
            reduction_db: 11.9,
        }));
        assert_eq!(tab.comparison, measured(-11.9, -63.9, -52.0));
        tab.event(&TestEvent::Done(TestOutcome::ClickTest { waited_ms: 812 }));
        tab.event(&TestEvent::Done(TestOutcome::NoiseListen { waited_ms: 0 }));
        assert_eq!(tab.click, measured(812.0, 0.0, 0.0));
        assert_eq!(tab.listen, measured(0.0, 0.0, 0.0));
        // a played recording measured nothing
        let before = tab.recording.clone();
        tab.event(&TestEvent::Done(TestOutcome::Played));
        assert_eq!(tab.recording, before);
    }

    #[test]
    fn a_cancelled_test_leaves_no_result_and_the_others_stay() {
        let mut tab = TestTab::default();
        tab.event(&TestEvent::Done(TestOutcome::NoiseFloor { dbfs: -60.0 }));
        tab.event(&TestEvent::Done(TestOutcome::ClickTest { waited_ms: 5 }));
        tab.event(&TestEvent::Done(TestOutcome::Cancelled(
            TestKind::NoiseFloor,
        )));
        assert!(!tab.noise_floor.present);
        assert!(tab.click.present);
    }

    #[test]
    fn starting_a_test_clears_its_result_and_the_problem_and_the_caution_goes_with_the_first_sound()
    {
        let mut tab = TestTab::default();
        tab.event(&TestEvent::Done(TestOutcome::NoiseFloor { dbfs: -60.0 }));
        tab.event(&TestEvent::Done(TestOutcome::Recorded {
            seconds: 5.0,
            rms_dbfs: -40.0,
            peak_dbfs: -20.0,
        }));
        tab.event(&TestEvent::Failed {
            kind: None,
            failure: failure(ErrorKind::Audio),
        });
        assert_eq!(tab.problem, TestProblem::Audio);

        tab.requested(TestJob::NoiseFloor);
        assert!(!tab.noise_floor.present);
        assert!(tab.recording.present, "another test's result stays");
        assert_eq!(tab.problem, TestProblem::None);
        assert!(!tab.sound_started, "a measurement plays nothing");

        for job in [TestJob::Play, TestJob::Click, TestJob::Listen] {
            let mut tab = TestTab::default();
            tab.requested(job);
            assert!(tab.sound_started, "{job:?}");
        }
        // playing keeps the recording's result
        let mut tab = TestTab::default();
        tab.event(&TestEvent::Done(TestOutcome::Recorded {
            seconds: 5.0,
            rms_dbfs: -40.0,
            peak_dbfs: -20.0,
        }));
        tab.requested(TestJob::Play);
        assert!(tab.recording.present);
    }

    #[test]
    fn a_failed_request_is_a_problem_by_what_the_user_can_do() {
        for (kind, problem) in [
            (ErrorKind::Communication, TestProblem::Communication),
            (ErrorKind::Audio, TestProblem::Audio),
            (ErrorKind::Invalid, TestProblem::Other),
            (ErrorKind::Unresponsive, TestProblem::Other),
        ] {
            let mut tab = TestTab::default();
            tab.event(&TestEvent::Failed {
                kind: None,
                failure: failure(kind),
            });
            assert_eq!(tab.problem, problem, "{kind:?}");
        }
    }

    #[test]
    fn the_level_is_shown_only_while_the_meter_runs() {
        let mut tab = TestTab::default();
        let level = Level {
            rms_dbfs: -52.0,
            peak_dbfs: -40.0,
        };
        tab.event(&TestEvent::Level(level));
        assert_eq!(tab.level, None, "the switch is off");
        tab.switched(true);
        tab.event(&TestEvent::Level(level));
        assert_eq!(tab.level, Some(-52.0), "the RMS level, not the peak");
        let metering = TestState {
            meter: true,
            ..TestState::default()
        };
        tab.state(&metering, true, true);
        assert_eq!(tab.level, Some(-52.0));
        // the engine says the meter is off
        tab.state(&TestState::default(), true, true);
        assert_eq!(tab.level, None);
        // a level that is still on its way when the user turns the switch off is not shown
        tab.event(&TestEvent::Level(level));
        tab.switched(false);
        tab.event(&TestEvent::Level(level));
        assert_eq!(tab.level, None);
    }

    #[test]
    fn progress_belongs_to_the_test_that_runs() {
        let mut tab = TestTab::default();
        let progress = TestProgress {
            kind: TestKind::NoiseFloor,
            step: EngineStep::Measuring,
            remaining_ms: Some(7_300),
        };
        tab.event(&TestEvent::Progress(progress));
        tab.state(&running(TestKind::NoiseFloor), true, true);
        assert_eq!(tab.progress, Some(progress));
        // the test ended: no countdown is left behind
        tab.state(&idle(), true, true);
        assert_eq!(tab.progress, None);
    }
}
