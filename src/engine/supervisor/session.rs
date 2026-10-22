//! The tests of the Test tab, run by the supervisor on its own clock: the level meter, noise
//! floor, recording and playback, the noise reduction A/B measurement, and the click test and
//! noise listening that need the output at rest.
//!
//! - The microphone is open only while the meter or a test needs it. Closing it is explicit
//!   ([`TestRequest::Meter`], the end of a test), and the engine then watches the output until it
//!   is idle again. Only the balanced mode changes the output for a capture: the headphone driver
//!   opens and the analog references come up, the references drop when the capture ends, and the
//!   driver is clamped after the stop decay time (800 ms by default). The other modes leave the
//!   output as it is.
//! - A test that runs for a while holds the dongle: the saved settings wait until it ends. The
//!   meter alone does not, so a setting can be changed and watched on the meter.
//! - The first seconds of a freshly opened microphone are not measured or recorded
//!   ([`WARMUP`]).

use std::time::{Duration, Instant};

use super::{Backend, Supervisor};
use crate::capture::{self, Level};
use crate::clip;
use crate::dsp;
use crate::engine::types::{
    ErrorKind, Event, Failure, TestEvent, TestKind, TestOutcome, TestProgress, TestRequest,
    TestState, TestStep,
};
use crate::engine::watch::Watched;
use crate::engine::{Clip, Microphone};
use crate::mode::{self, Mode};
use crate::patches::{APPLIED_NR, APPLIERS};
use crate::protocol::read_mem;
use crate::{Error, Result};

/// How often a running test is looked at; one block of the microphone's level.
const TICK: Duration = Duration::from_millis(capture::BLOCK_MS as u64);
/// How often a running test reports its countdown.
const PROGRESS: Duration = Duration::from_millis(100);
/// What a microphone that was just opened delivers is left out of every measurement and
/// recording: digital silence and a click, and in balanced mode a stretch of higher levels that
/// ended at the latest 3.1 s after the first samples in the meter runs on the device. Those runs
/// were not made in a quiet room, so the length is provisional (docs/firmware-notes.md).
pub(in crate::engine) const WARMUP: Duration = Duration::from_secs(4);
/// How long the noise floor is left to settle after noise reduction was switched.
const SWITCH_SETTLE: Duration = Duration::from_secs(1);
/// How long the output may take to reach idle before the wait is given up.
pub(in crate::engine) const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the output is looked at while it settles after the microphone was closed.
const IDLE_POLL: Duration = Duration::from_millis(100);
/// The quiet after the output reached idle, before the click test starts its stream.
const REST: Duration = Duration::from_millis(500);
/// Silence after the mute delay of the mode and before the tone, so that the whole tone is heard.
const LEAD_MARGIN_MS: u32 = 200;
/// Silence after a clip, so that its stream ends with the codec idle.
const TAIL_MS: u32 = 300;
/// A microphone that delivers nothing for this long is taken as stopped.
const STALL: Duration = Duration::from_secs(3);

fn ms(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

struct Open<M> {
    stream: M,
    since: Instant,
    /// When the stream last delivered a block.
    last_data: Instant,
}

enum Measuring {
    /// A plain noise floor measurement.
    Plain,
    /// The first half of the A/B measurement, noise reduction on.
    WithNr,
    /// The second half, noise reduction off; holds the first half's result.
    WithoutNr(f32),
}

enum Step<P> {
    WarmUp {
        until: Instant,
    },
    Measure {
        which: Measuring,
        need: usize,
        levels: Vec<Level>,
    },
    Record {
        need: usize,
        samples: Vec<i16>,
    },
    SwitchNr {
        until: Instant,
        with: f32,
    },
    WaitIdle {
        since: Instant,
    },
    Rest {
        until: Instant,
    },
    Play {
        clip: P,
        since: Instant,
        total_ms: u32,
    },
}

struct Job<P> {
    kind: TestKind,
    step: Step<P>,
    /// Seconds asked for (noise floor, recording) or per half (A/B).
    seconds: u8,
    /// Noise reduction on the dongle when an A/B measurement started, to put back.
    nr_original: Option<bool>,
    /// How long the output took to reach idle.
    waited_ms: u32,
    mode: Option<Mode>,
    soft_start_ms: Option<u16>,
}

struct Settling {
    since: Instant,
    last_poll: Option<Instant>,
}

pub(super) struct Tests<B: Backend> {
    meter: bool,
    capture: Option<Open<B::Capture>>,
    job: Option<Job<B::Playback>>,
    recording: Option<Vec<i16>>,
    settling: Option<Settling>,
    /// The dongle, open for the reads of a test.
    dev: Option<Watched<B::Dev>>,
    next_tick: Option<Instant>,
    last_progress: Option<(TestStep, Instant)>,
}

impl<B: Backend> Default for Tests<B> {
    fn default() -> Self {
        Tests {
            meter: false,
            capture: None,
            job: None,
            recording: None,
            settling: None,
            dev: None,
            next_tick: None,
            last_progress: None,
        }
    }
}

impl<B: Backend> Tests<B> {
    /// True while a test that needs the dongle for itself runs.
    pub(super) fn holds_the_dongle(&self) -> bool {
        self.job.is_some()
    }

    /// Lets go of the dongle that the tests had open, so that a settings pass has it to itself.
    pub(super) fn release_dongle(&mut self) {
        self.dev = None;
    }

    pub(super) fn next_tick(&self) -> Option<Instant> {
        self.next_tick
    }

    pub(super) fn state(&self) -> TestState {
        TestState {
            meter: self.meter,
            running: self.job.as_ref().map(|job| job.kind),
            has_recording: self.recording.is_some(),
        }
    }

    fn active(&self) -> bool {
        self.capture.is_some() || self.job.is_some() || self.settling.is_some()
    }
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure {
        kind: ErrorKind::Invalid,
        message: message.into(),
    }
}

impl<B: Backend> Supervisor<B> {
    fn emit_test(&mut self, event: TestEvent) {
        self.emit(Event::Test(event));
    }

    fn test_failed(&mut self, kind: Option<TestKind>, failure: Failure) {
        self.note(format!(
            "test {kind:?} failed ({}): {}",
            failure.kind, failure.message
        ));
        self.emit_test(TestEvent::Failed { kind, failure });
    }

    /// Reports a countdown, when the step changed or enough time has passed.
    fn progress(&mut self, kind: TestKind, step: TestStep, remaining: Option<u32>, now: Instant) {
        let due = match self.tests.last_progress {
            Some((last, at)) => last != step || now.saturating_duration_since(at) >= PROGRESS,
            None => true,
        };
        if due {
            self.tests.last_progress = Some((step, now));
            self.emit_test(TestEvent::Progress(TestProgress {
                kind,
                step,
                remaining_ms: remaining,
            }));
        }
    }

    /// The mode the dongle is in, as far as the engine has seen.
    fn known_mode(&self) -> Option<Mode> {
        self.device
            .as_ref()
            .and_then(|device| device.mode)
            .or(self.cfg.mode)
    }

    fn test_dev(&mut self) -> Result<&mut Watched<B::Dev>> {
        if self.tests.dev.is_none() {
            let _call = self.watch.enter("opening the dongle");
            let dev = self.backend.open()?;
            self.tests.dev = Some(Watched::new(dev, self.watch.clone()));
        }
        Ok(self.tests.dev.as_mut().expect("opened above"))
    }

    fn wake_tests(&mut self, now: Instant) {
        self.tests.next_tick = Some(self.tests.next_tick.map_or(now, |at| at.min(now)));
    }

    fn open_microphone(&mut self, now: Instant) -> Result<()> {
        if self.tests.capture.is_some() {
            return Ok(());
        }
        let stream = {
            let _call = self.watch.enter("opening the microphone");
            self.backend.open_capture()?
        };
        self.note("the microphone was opened for a test");
        // opening takes a while: the warm-up counts from the end of it
        let opened = self.now();
        self.tests.capture = Some(Open {
            stream,
            since: opened,
            last_data: opened,
        });
        self.wake_tests(now);
        Ok(())
    }

    /// Closes the microphone unless the meter or the running test still needs it, and then
    /// watches the output settle.
    fn close_microphone(&mut self, now: Instant) {
        let needed = self.tests.meter
            || self
                .tests
                .job
                .as_ref()
                .is_some_and(|job| job.kind.uses_microphone());
        if needed || self.tests.capture.take().is_none() {
            return;
        }
        self.note("the microphone was closed");
        // Only balanced mode (and a mode that is not known) changes the output while the codec
        // captures.
        if matches!(self.known_mode(), None | Some(Mode::Balanced)) {
            self.tests.settling = Some(Settling {
                since: self.now(),
                last_poll: None,
            });
            self.wake_tests(now);
        }
    }

    pub(super) fn test_request(&mut self, request: TestRequest) {
        let now = self.now();
        if !self.present
            && !matches!(request, TestRequest::Cancel | TestRequest::Meter(false))
            && self.tests.job.is_none()
        {
            return self.test_failed(
                None,
                Failure {
                    kind: ErrorKind::Communication,
                    message: "the dongle is not connected".into(),
                },
            );
        }
        match request {
            TestRequest::Meter(true)
                if self
                    .tests
                    .job
                    .as_ref()
                    .is_some_and(|job| !job.kind.uses_microphone()) =>
            {
                self.test_failed(
                    None,
                    invalid("the output test needs the microphone closed; cancel it first"),
                );
            }
            TestRequest::Meter(on) => self.set_meter(on, now),
            TestRequest::Cancel => self.cancel_job(now),
            _ if self.tests.job.is_some() => {
                self.test_failed(None, invalid("another test is running; cancel it first"));
            }
            TestRequest::NoiseFloor { seconds } => self.start_measurement(
                TestKind::NoiseFloor,
                seconds,
                TestRequest::MAX_NOISE_FLOOR_SECONDS,
                now,
            ),
            TestRequest::Record { seconds } => self.start_measurement(
                TestKind::Record,
                seconds,
                TestRequest::MAX_RECORD_SECONDS,
                now,
            ),
            TestRequest::NoiseReductionAb { seconds } => self.start_measurement(
                TestKind::NoiseReductionAb,
                seconds,
                TestRequest::MAX_AB_SECONDS,
                now,
            ),
            TestRequest::PlayRecording => self.start_playback(now),
            TestRequest::ClickTest => self.start_waiting(TestKind::ClickTest, now),
            TestRequest::NoiseListen => self.start_waiting(TestKind::NoiseListen, now),
        }
    }

    fn set_meter(&mut self, on: bool, now: Instant) {
        self.tests.meter = on;
        if on {
            if let Err(error) = self.open_microphone(now) {
                self.tests.meter = false;
                self.test_failed(None, Failure::from(error));
            }
        } else {
            self.close_microphone(now);
        }
    }

    fn start_measurement(&mut self, kind: TestKind, seconds: u8, max: u8, now: Instant) {
        if !(1..=max).contains(&seconds) {
            return self.test_failed(
                Some(kind),
                invalid(format!(
                    "the duration must be 1-{max} seconds, got {seconds}"
                )),
            );
        }
        if let Err(error) = self.begin_measurement(kind, seconds, now) {
            self.abandon_job(kind, error, now);
        }
    }

    fn begin_measurement(&mut self, kind: TestKind, seconds: u8, now: Instant) -> Result<()> {
        self.tests.settling = None;
        self.open_microphone(now)?;
        let since = self.tests.capture.as_ref().expect("opened above").since;
        let mut job = Job {
            kind,
            step: Step::WarmUp {
                until: since + WARMUP,
            },
            seconds,
            nr_original: None,
            waited_ms: 0,
            mode: None,
            soft_start_ms: None,
        };
        if kind == TestKind::NoiseReductionAb {
            let original = read_mem(self.test_dev()?, crate::patches::DSP_ENABLE, 1)?[0]
                & crate::patches::DSP_NR
                != 0;
            // From here on the setting is to be put back, whatever happens.
            job.nr_original = Some(original);
            if let Err(error) = self.switch_noise_reduction(true) {
                self.put_back_noise_reduction(&job);
                return Err(error);
            }
        }
        self.progress(kind, TestStep::WarmingUp, None, now);
        self.tests.job = Some(job);
        self.wake_tests(now);
        Ok(())
    }

    fn start_playback(&mut self, now: Instant) {
        let kind = TestKind::PlayRecording;
        if self.tests.recording.is_none() {
            return self.test_failed(Some(kind), invalid("there is no recording to play"));
        }
        if let Err(error) = self.begin_waiting(kind, now) {
            self.abandon_job(kind, error, now);
        }
    }

    fn start_waiting(&mut self, kind: TestKind, now: Instant) {
        if let Err(error) = self.begin_waiting(kind, now) {
            self.abandon_job(kind, error, now);
        }
    }

    /// Starts a test that needs no microphone: the click test, noise listening and playback.
    fn begin_waiting(&mut self, kind: TestKind, now: Instant) -> Result<()> {
        self.tests.meter = false;
        self.tests.capture = None;
        self.tests.settling = None;
        // the wait for idle counts from the end of the microphone's closing
        let closed = self.now();
        let state = mode::read_state(self.test_dev()?)?;
        let step = if kind == TestKind::PlayRecording {
            Step::Rest { until: closed }
        } else {
            Step::WaitIdle { since: closed }
        };
        self.tests.job = Some(Job {
            kind,
            step,
            seconds: 0,
            nr_original: None,
            waited_ms: 0,
            mode: state.mode(),
            soft_start_ms: state.settle_ms(),
        });
        self.wake_tests(now);
        Ok(())
    }

    /// A request that failed before its test started.
    fn abandon_job(&mut self, kind: TestKind, error: Error, now: Instant) {
        self.tests.job = None;
        self.test_failed(Some(kind), Failure::from(&error));
        self.after_job(now);
    }

    /// Ends the running test, if there is one, and puts back what it changed.
    fn cancel_job(&mut self, now: Instant) {
        let Some(job) = self.tests.job.take() else {
            return;
        };
        let kind = job.kind;
        self.put_back_noise_reduction(&job);
        self.emit_test(TestEvent::Done(TestOutcome::Cancelled(kind)));
        drop(job);
        self.after_job(now);
    }

    fn put_back_noise_reduction(&mut self, job: &Job<B::Playback>) {
        if let Some(original) = job.nr_original
            && let Err(error) = self.switch_noise_reduction(original)
        {
            self.warn(format!(
                "could not put noise reduction back after the test: {error}"
            ));
        }
    }

    /// What follows every test: the microphone closes unless the meter wants it, and the dongle
    /// is let go.
    fn after_job(&mut self, now: Instant) {
        self.tests.last_progress = None;
        self.close_microphone(now);
        if self.tests.settling.is_none() {
            self.tests.dev = None;
        }
        self.reschedule_tests();
    }

    fn reschedule_tests(&mut self) {
        if !self.tests.active() {
            self.tests.next_tick = None;
        }
    }

    /// Stops everything a test has open, because the dongle or the engine went away.
    pub(super) fn abort_tests(&mut self, reason: &str) {
        let was_running = self.tests.job.is_some() || self.tests.meter;
        let kind = self.tests.job.as_ref().map(|job| job.kind);
        let job = self.tests.job.take();
        if let Some(job) = &job {
            self.put_back_noise_reduction(job);
        }
        drop(job);
        self.tests.meter = false;
        self.tests.capture = None;
        self.tests.settling = None;
        self.tests.dev = None;
        self.tests.next_tick = None;
        self.tests.last_progress = None;
        if was_running {
            self.test_failed(
                kind,
                Failure {
                    kind: ErrorKind::Communication,
                    message: reason.to_string(),
                },
            );
        }
    }

    /// Stops a test and everything it opened, after an error.
    fn fail_tests(&mut self, error: Error) {
        let now = self.now();
        let failure = Failure::from(&error);
        if let Some(job) = self.tests.job.take() {
            self.put_back_noise_reduction(&job);
            let kind = job.kind;
            drop(job);
            self.test_failed(Some(kind), failure);
        } else {
            self.test_failed(None, failure);
        }
        self.tests.meter = false;
        self.tests.capture = None;
        self.tests.last_progress = None;
        self.start_settling_after_loss(now);
        self.reschedule_tests();
    }

    fn start_settling_after_loss(&mut self, now: Instant) {
        if matches!(self.known_mode(), None | Some(Mode::Balanced)) {
            self.tests.settling = Some(Settling {
                since: now,
                last_poll: None,
            });
        }
    }

    /// Switches noise reduction on the dongle, reporting a stream event near the reload request as
    /// the settings pass does.
    fn switch_noise_reduction(&mut self, on: bool) -> Result<()> {
        let applied = dsp::set_noise_reduction(self.test_dev()?, on)?;
        if let Some(event) = applied.stream_event {
            self.warn(event.to_string());
        }
        Ok(())
    }

    /// Fails unless the ROM applies noise reduction as `on` says.
    fn check_noise_reduction(&mut self, on: bool) -> Result<()> {
        let applied = read_mem(self.test_dev()?, APPLIERS, 1)?[0] & APPLIED_NR != 0;
        if applied == on {
            Ok(())
        } else {
            Err(Error::Audio(format!(
                "noise reduction did not switch {} in the codec",
                if on { "on" } else { "off" }
            )))
        }
    }

    /// Runs what is due.
    pub(super) fn tick_tests(&mut self, now: Instant) {
        if self.tests.next_tick.is_none_or(|at| now < at) {
            return;
        }
        self.tests.next_tick = Some(now + TICK);
        let mut poll = capture::Poll::default();
        if let Some(open) = &mut self.tests.capture {
            match open.stream.poll() {
                Ok(got) => {
                    if !got.levels.is_empty() {
                        open.last_data = now;
                    }
                    poll = got;
                }
                Err(error) => return self.fail_tests(error),
            }
        }
        if self.tests.meter {
            for level in &poll.levels {
                self.emit_test(TestEvent::Level(*level));
            }
        }
        if let Some(job) = self.tests.job.take() {
            self.advance(job, now, poll);
        }
        self.tick_settling(now);
        self.reschedule_tests();
    }

    fn advance(&mut self, mut job: Job<B::Playback>, now: Instant, poll: capture::Poll) {
        match self.advance_step(&mut job, now, poll) {
            Ok(None) => self.tests.job = Some(job),
            Ok(Some(outcome)) => {
                self.put_back_noise_reduction(&job);
                self.emit_test(TestEvent::Done(outcome));
                drop(job);
                self.after_job(now);
            }
            Err(error) => {
                self.put_back_noise_reduction(&job);
                let kind = job.kind;
                drop(job);
                self.test_failed(Some(kind), Failure::from(&error));
                self.after_job(now);
            }
        }
    }

    fn stalled(&self, now: Instant) -> Result<()> {
        match &self.tests.capture {
            Some(open) if now.saturating_duration_since(open.last_data) >= STALL => {
                Err(Error::Audio("the microphone delivers no data".into()))
            }
            _ => Ok(()),
        }
    }

    /// The step that follows the warm-up of `job`.
    fn after_warmup(&mut self, job: &Job<B::Playback>) -> Result<Step<B::Playback>> {
        let blocks = usize::from(job.seconds) * (1000 / capture::BLOCK_MS as usize);
        Ok(match job.kind {
            TestKind::Record => {
                if let Some(open) = &mut self.tests.capture {
                    open.stream.record(true);
                }
                Step::Record {
                    need: usize::from(job.seconds) * capture::RATE as usize,
                    samples: Vec::new(),
                }
            }
            TestKind::NoiseReductionAb => {
                self.check_noise_reduction(true)?;
                Step::Measure {
                    which: Measuring::WithNr,
                    need: blocks,
                    levels: Vec::new(),
                }
            }
            _ => Step::Measure {
                which: Measuring::Plain,
                need: blocks,
                levels: Vec::new(),
            },
        })
    }

    fn advance_step(
        &mut self,
        job: &mut Job<B::Playback>,
        now: Instant,
        poll: capture::Poll,
    ) -> Result<Option<TestOutcome>> {
        let kind = job.kind;
        let mut next = None;
        let mut outcome = None;
        // the steps that look at the dongle are done after the match, which holds the step
        match &mut job.step {
            Step::WarmUp { until } => {
                if now >= *until {
                    next = Some(self.after_warmup(job)?);
                } else {
                    let left = ms(*until - now);
                    self.stalled(now)?;
                    self.progress(kind, TestStep::WarmingUp, Some(left), now);
                }
            }
            Step::Measure {
                which,
                need,
                levels,
            } => {
                levels.extend(poll.levels);
                if levels.len() >= *need {
                    let dbfs = capture::median_rms_dbfs(&levels[..*need]).unwrap_or(f32::NAN);
                    match which {
                        Measuring::Plain => outcome = Some(TestOutcome::NoiseFloor { dbfs }),
                        Measuring::WithNr => {
                            self.switch_noise_reduction(false)?;
                            next = Some(Step::SwitchNr {
                                until: now + SWITCH_SETTLE,
                                with: dbfs,
                            });
                        }
                        Measuring::WithoutNr(with) => {
                            let with = *with;
                            outcome = Some(TestOutcome::NoiseReductionAb {
                                with_dbfs: with,
                                without_dbfs: dbfs,
                                reduction_db: dbfs - with,
                            });
                        }
                    }
                } else {
                    let step = match which {
                        Measuring::Plain => TestStep::Measuring,
                        Measuring::WithNr => TestStep::MeasuringWithNoiseReduction,
                        Measuring::WithoutNr(_) => TestStep::MeasuringWithoutNoiseReduction,
                    };
                    let left = (*need - levels.len()) as u32 * capture::BLOCK_MS;
                    self.stalled(now)?;
                    self.progress(kind, step, Some(left), now);
                }
            }
            Step::Record { need, samples } => {
                samples.extend(poll.samples);
                if samples.len() >= *need {
                    samples.truncate(*need);
                    if let Some(open) = &mut self.tests.capture {
                        open.stream.record(false);
                    }
                    let level = Level::of(samples);
                    let seconds = samples.len() as f32 / capture::RATE as f32;
                    self.tests.recording = Some(std::mem::take(samples));
                    outcome = Some(TestOutcome::Recorded {
                        seconds,
                        rms_dbfs: level.rms_dbfs,
                        peak_dbfs: level.peak_dbfs,
                    });
                } else {
                    let left =
                        ((*need - samples.len()) as u64 * 1000 / u64::from(capture::RATE)) as u32;
                    self.stalled(now)?;
                    self.progress(kind, TestStep::Recording, Some(left), now);
                }
            }
            Step::SwitchNr { until, with } => {
                if now >= *until {
                    let with = *with;
                    self.check_noise_reduction(false)?;
                    next = Some(Step::Measure {
                        which: Measuring::WithoutNr(with),
                        need: usize::from(job.seconds) * (1000 / capture::BLOCK_MS as usize),
                        levels: Vec::new(),
                    });
                } else {
                    let left = ms(*until - now);
                    self.progress(kind, TestStep::SwitchingNoiseReduction, Some(left), now);
                }
            }
            Step::WaitIdle { since } => {
                let since = *since;
                let output = mode::read_output(self.test_dev()?)?;
                if output.is_idle(job.mode) {
                    job.waited_ms = ms(now - since);
                    if kind == TestKind::NoiseListen {
                        outcome = Some(TestOutcome::NoiseListen {
                            waited_ms: job.waited_ms,
                        });
                    } else {
                        next = Some(Step::Rest { until: now + REST });
                    }
                } else if now - since >= IDLE_TIMEOUT {
                    return Err(Error::Audio(format!(
                        "the output did not become idle within {} s ($0F18={:#04x} $0F1A={:#04x} \
                         $0F1B={:#04x} $0F3E={:#04x} capture {:#04x} playback {:#04x}); another \
                         program may be using the dongle",
                        IDLE_TIMEOUT.as_secs(),
                        output.dac_enable,
                        output.hp_driver,
                        output.reg_0f1b,
                        output.refs,
                        output.capture_stream,
                        output.playback_stream
                    )));
                } else {
                    self.progress(kind, TestStep::WaitingForIdle, None, now);
                }
            }
            Step::Rest { until } => {
                if now >= *until {
                    let (clip, total_ms) = self.clip_for(job);
                    let playback = {
                        let _call = self.watch.enter("starting the clip");
                        self.backend.play(clip)?
                    };
                    next = Some(Step::Play {
                        clip: playback,
                        since: now,
                        total_ms,
                    });
                } else {
                    let left = ms(*until - now);
                    self.progress(kind, TestStep::Resting, Some(left), now);
                }
            }
            Step::Play {
                clip,
                since,
                total_ms,
            } => {
                if clip.is_finished()? {
                    outcome = Some(if kind == TestKind::PlayRecording {
                        TestOutcome::Played
                    } else {
                        TestOutcome::ClickTest {
                            waited_ms: job.waited_ms,
                        }
                    });
                } else {
                    let left = total_ms.saturating_sub(ms(now - *since));
                    self.progress(kind, TestStep::Playing, Some(left), now);
                }
            }
        }
        if let Some(step) = next {
            job.step = step;
            self.tests.last_progress = None;
        }
        Ok(outcome)
    }

    /// The clip for the test that is about to play, and its length in ms: silence for the mute
    /// delay of the mode and a margin, then the tone or the recording, then a tail.
    fn clip_for(&mut self, job: &Job<B::Playback>) -> (Vec<i16>, u32) {
        let lead = mode::mute_delay_ms(job.mode, job.soft_start_ms) + LEAD_MARGIN_MS;
        let body = if job.kind == TestKind::PlayRecording {
            let mut recording = self.tests.recording.clone().unwrap_or_default();
            clip::limit_peak(&mut recording, clip::PLAYBACK_PEAK_DBFS);
            recording
        } else {
            clip::test_tone()
        };
        let clip = clip::pad(&body, lead, TAIL_MS);
        let total = (clip.len() as u64 * 1000 / u64::from(crate::audio::PLAY_RATE)) as u32;
        (clip, total)
    }

    /// Watches the output reach idle after the microphone was closed.
    fn tick_settling(&mut self, now: Instant) {
        let Some(settling) = &mut self.tests.settling else {
            return;
        };
        if settling
            .last_poll
            .is_some_and(|at| now.saturating_duration_since(at) < IDLE_POLL)
        {
            return;
        }
        settling.last_poll = Some(now);
        let since = settling.since;
        let mode = self.known_mode();
        let reached = self
            .test_dev()
            .and_then(mode::read_output)
            .map(|output| output.is_idle(mode));
        match reached {
            Ok(true) => {
                self.note(format!(
                    "the output is idle again {} ms after the microphone was closed",
                    ms(now - since)
                ));
            }
            Ok(false) if now - since < IDLE_TIMEOUT => return,
            Ok(false) => self.warn(format!(
                "the output did not return to idle within {} s after the microphone was closed",
                IDLE_TIMEOUT.as_secs()
            )),
            Err(error) => self.note(format!(
                "the output was not watched after the microphone was closed: {error}"
            )),
        }
        self.tests.settling = None;
        if self.tests.job.is_none() {
            self.tests.dev = None;
        }
    }
}

#[cfg(test)]
mod tests;
