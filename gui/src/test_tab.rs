//! The Test page: what it asks of the engine, and how the engine's answers are shown. The tests
//! themselves, and the rules for which of them may run together, are the engine's (docs/engine.md);
//! the page only follows them.

use cxtune::engine::{
    Command, ErrorKind, TestEvent, TestKind, TestOutcome, TestProgress, TestRequest, TestState,
    TestStep as EngineStep,
};

use crate::ui::{Measured, Model, TestJob, TestProblem, TestStep};

/// The index of the Test page in the window.
pub const TAB: i32 = 3;
/// How long the measurements run, in seconds.
pub const NOISE_FLOOR_SECONDS: u8 = 10;
pub const RECORD_SECONDS: u8 = 5;
/// Each half of the noise reduction comparison.
pub const COMPARISON_SECONDS: u8 = 10;

/// What the page quotes at rest, for a level that no meter delivered.
const NO_LEVEL_DB: f32 = -60.0;

/// Whether the microphone meter should run. It runs only while the Test page is shown in a window
/// that is shown, and the dongle is there. The output tests need the microphone closed (the engine
/// refuses the meter while one runs and closes it when one starts), so it stays off until they
/// end; the measurements keep it as it is.
pub fn meter_wanted(tab: i32, window_shown: bool, usable: bool, running: Option<TestKind>) -> bool {
    tab == TAB && window_shown && usable && running.is_none_or(TestKind::uses_microphone)
}

/// Tells the engine when the meter is wanted and when it is not, once for each change.
#[derive(Default)]
pub struct MeterAsk {
    asked: bool,
}

impl MeterAsk {
    pub fn sync(&mut self, wanted: bool) -> Option<Command> {
        if wanted == self.asked {
            return None;
        }
        self.asked = wanted;
        Some(Command::Test(TestRequest::Meter(wanted)))
    }

    /// A new engine has not been asked anything.
    pub fn reset(&mut self) {
        self.asked = false;
    }
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
/// the last problem and the level of the meter.
#[derive(Default)]
pub struct TestTab {
    running: Option<TestKind>,
    progress: Option<TestProgress>,
    has_recording: bool,
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
    /// What the engine's state says: which test runs, whether the meter is on, whether there is a
    /// recording.
    pub fn state(&mut self, state: &TestState) {
        self.running = state.running;
        self.has_recording = state.has_recording;
        if state.running.is_none() {
            self.progress = None;
        }
        if !state.meter {
            self.level = None;
        }
    }

    pub fn event(&mut self, event: &TestEvent) {
        match event {
            TestEvent::Level(level) => self.level = Some(level.rms_dbfs),
            TestEvent::Progress(progress) => self.progress = Some(*progress),
            TestEvent::Done(outcome) => {
                self.progress = None;
                self.done(outcome);
            }
            TestEvent::Failed { failure, .. } => {
                self.progress = None;
                self.problem = problem_of(failure.kind);
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

    /// The user started `job`: its old result and the last problem are gone, and a test that
    /// plays a sound has been started.
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
        self.sound_started |= matches!(job, TestJob::Play | TestJob::Click | TestJob::Listen);
    }

    /// The test that runs, as the engine's state says.
    pub fn running(&self) -> Option<TestKind> {
        self.running
    }

    pub fn show(&self, model: &Model) {
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
    use cxtune::engine::Failure;

    use super::*;

    fn failure(kind: ErrorKind) -> Failure {
        Failure {
            kind,
            message: "x".into(),
        }
    }

    #[test]
    fn the_meter_runs_only_on_the_test_page_of_a_shown_window_with_a_dongle() {
        assert!(meter_wanted(TAB, true, true, None));
        assert!(!meter_wanted(0, true, true, None), "another page");
        assert!(!meter_wanted(TAB + 1, true, true, None), "another page");
        assert!(!meter_wanted(TAB, false, true, None), "a hidden window");
        assert!(!meter_wanted(TAB, true, false, None), "no dongle");
    }

    #[test]
    fn the_meter_waits_for_an_output_test_and_keeps_running_through_a_measurement() {
        for kind in [
            TestKind::NoiseFloor,
            TestKind::Record,
            TestKind::NoiseReductionAb,
        ] {
            assert!(meter_wanted(TAB, true, true, Some(kind)), "{kind:?}");
        }
        for kind in [
            TestKind::PlayRecording,
            TestKind::ClickTest,
            TestKind::NoiseListen,
        ] {
            assert!(!meter_wanted(TAB, true, true, Some(kind)), "{kind:?}");
        }
    }

    #[test]
    fn the_engine_is_asked_once_for_each_change_of_what_is_wanted() {
        let meter = |on| Some(Command::Test(TestRequest::Meter(on)));
        let mut ask = MeterAsk::default();
        assert_eq!(ask.sync(false), None);
        // entering the page
        assert_eq!(ask.sync(true), meter(true));
        assert_eq!(ask.sync(true), None);
        // an output test starts and ends
        assert_eq!(ask.sync(false), meter(false));
        assert_eq!(ask.sync(false), None);
        assert_eq!(ask.sync(true), meter(true));
        // leaving the page
        assert_eq!(ask.sync(false), meter(false));
        // a new engine knows nothing of what was asked before
        assert_eq!(ask.sync(true), meter(true));
        ask.reset();
        assert_eq!(ask.sync(true), meter(true));
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
    fn the_level_is_shown_only_while_the_engine_says_the_meter_is_on() {
        let mut tab = TestTab::default();
        let level = Level {
            rms_dbfs: -52.0,
            peak_dbfs: -40.0,
        };
        tab.event(&TestEvent::Level(level));
        assert_eq!(tab.level, Some(-52.0), "the RMS level, not the peak");
        tab.state(&TestState {
            meter: true,
            ..TestState::default()
        });
        assert_eq!(tab.level, Some(-52.0));
        tab.state(&TestState::default());
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
        tab.state(&TestState {
            running: Some(TestKind::NoiseFloor),
            ..TestState::default()
        });
        assert_eq!(tab.progress, Some(progress));
        // the test ended: no countdown is left behind
        tab.state(&TestState::default());
        assert_eq!(tab.progress, None);
    }
}
