//! The engine's state machine: what to do about the dongle, and when.
//!
//! It owns the saved settings and every decision, and does nothing by itself: the worker thread
//! calls [`Supervisor::command`] with what arrived and [`Supervisor::step`] when
//! [`Supervisor::next_wake`] has passed. Time comes from an injected clock, so the tests drive it
//! with a fake device and a clock they advance by hand.
//!
//! - **Presence** is polled (enumeration only, no device I/O). A dongle that appears is left
//!   alone for [`SETTLE`] so that its firmware can load the patch; one that disappears again
//!   before that never gets touched, so a flapping cable causes no writes and no errors.
//! - **Verification** reads the dongle once per [`VERIFY`] and compares it with the saved
//!   settings. Only a difference leads to writes, so a dongle that is in order is only read.
//! - **Commands** change the saved settings at once and the dongle at most [`COALESCE`] later,
//!   with the latest values, however many arrived.
//! - **Failures** are retried by a schedule that depends on their kind ([`retry_delay`]) and
//!   reported once while they persist. A failure while the dongle is gone is a disconnect.
//! - **Drift** that nobody asked for is corrected and reported, a few times; a setting that
//!   something else keeps changing is then left alone ([`MAX_CORRECTIONS`]). A dongle that holds
//!   the vendor's settings again has restarted between two polls; that is handled like an
//!   arrival and is never drift, and a dongle that does it again and again is also reported.
//! - **The settings file** is written shortly after a change and written again, with growing
//!   pauses, for as long as that fails.
//! - **Tests** of the Test tab ([`session`]) run on the same clock. While one that needs the
//!   dongle for itself runs, the saved settings wait.

use std::path::PathBuf;
use std::time::{Duration, Instant};

mod session;

use self::session::Tests;
use super::Backend;
use super::types::{Command, ErrorKind, Event, Failure, Phase, Readback, Snapshot, without};
use super::watch::{CallWatch, Watched};
use crate::config::{self, Config};
use crate::dsp::Attempt;
use crate::{Error, mode, patches, settings};

/// How often the presence of the dongle is checked.
pub(super) const POLL: Duration = Duration::from_secs(1);
/// How long a dongle that appeared (or a computer that woke up) is left alone, so that the
/// firmware can load its patch.
pub(super) const SETTLE: Duration = Duration::from_secs(2);
/// How long after the first of a burst of commands the dongle and the settings file are written.
pub(super) const COALESCE: Duration = Duration::from_millis(150);
/// How often a connected dongle is compared with the saved settings.
pub(super) const VERIFY: Duration = Duration::from_secs(15);
/// How many times something that changed a setting without cxtune is corrected within
/// [`DRIFT_WINDOW`] before the setting is left alone.
pub(super) const MAX_CORRECTIONS: usize = 3;
/// The time in which [`MAX_CORRECTIONS`] corrections of one setting are allowed, and in which
/// restarts of the dongle are counted.
pub(super) const DRIFT_WINDOW: Duration = Duration::from_secs(10 * 60);
/// From this many restarts of the dongle within [`DRIFT_WINDOW`], each further one is reported as
/// a warning. The dongle is restored all the same.
pub(super) const WARN_RESTARTS: usize = 3;

/// How long to wait before the next attempt after `failures` failed attempts in a row (at least
/// one), or `None` to stop until something changes: a command, a re-apply, or the dongle coming
/// back.
pub(super) fn retry_delay(kind: ErrorKind, failures: u32) -> Option<Duration> {
    let nth = failures.saturating_sub(1) as usize;
    let backoff = |first: u64, cap: u64| Some(Duration::from_secs((first << nth.min(16)).min(cap)));
    let steps = |secs: &[u64]| secs.get(nth).map(|&s| Duration::from_secs(s));
    match kind {
        ErrorKind::Communication => backoff(2, 30),
        ErrorKind::Audio => backoff(5, 60),
        // a patch that is still loading looks unrecognised for a moment; a lasting refusal is
        // not worth more than a few looks
        ErrorKind::Refused => steps(&[5, 15, 60]),
        ErrorKind::Verify => steps(&[10, 60]),
        ErrorKind::Config => backoff(2, 30),
        ErrorKind::Invalid | ErrorKind::Drift | ErrorKind::Unresponsive | ErrorKind::Fatal => None,
    }
}

/// How a pass over the dongle ended without a failure.
enum Pass {
    /// The dongle holds the saved settings, found so or brought to them.
    Done,
    /// The dongle had restarted since the last pass; it is treated like one that has just
    /// appeared, and the pass has been scheduled again.
    Restarted,
}

pub(super) struct Supervisor<B: Backend> {
    backend: B,
    cfg: Config,
    /// What the settings file holds, as far as this process knows.
    saved: Config,
    cfg_path: PathBuf,
    sink: Box<dyn FnMut(Event)>,
    log: Box<dyn FnMut(&str)>,
    clock: Box<dyn Fn() -> Instant>,
    watch: CallWatch,

    present: bool,
    next_poll: Instant,
    enumeration_error: Option<String>,
    /// When the dongle is next read and, if it differs, written. `None`: nothing is scheduled.
    work_at: Option<Instant>,
    /// When the settings file is next written.
    save_at: Option<Instant>,
    /// Failed attempts to write the settings file in a row.
    save_failures: u32,
    save_error: Option<Failure>,
    /// The next pass applies the saved settings whether or not the dongle seems to hold them.
    force: bool,
    /// The next opportunity reads the dongle.
    refresh: bool,
    /// Failed passes in a row.
    failures: u32,
    /// The settings that the dongle was last found to hold, if it has held them since it
    /// connected. A difference while these are still the chosen settings is not the user's doing.
    synced: Option<Config>,
    /// When settings were corrected after something else changed them.
    corrections: Vec<(&'static str, Instant)>,
    /// When the dongle was found to have restarted between two polls.
    restarts: Vec<Instant>,
    /// Settings that are no longer corrected because something keeps changing them.
    muted: Vec<&'static str>,
    drift_error: Option<Failure>,
    tests: Tests<B>,

    phase: Phase,
    device: Option<Readback>,
    error: Option<Failure>,
    published: Option<Snapshot>,
}

impl<B: Backend> Supervisor<B> {
    pub(super) fn new(
        backend: B,
        cfg: Config,
        cfg_path: PathBuf,
        sink: Box<dyn FnMut(Event)>,
        log: Box<dyn FnMut(&str)>,
        clock: Box<dyn Fn() -> Instant>,
        watch: CallWatch,
    ) -> Self {
        let now = clock();
        let mut supervisor = Supervisor {
            backend,
            saved: cfg.clone(),
            cfg,
            cfg_path,
            sink,
            log,
            clock,
            watch,
            present: false,
            next_poll: now,
            enumeration_error: None,
            work_at: None,
            save_at: None,
            save_failures: 0,
            save_error: None,
            force: false,
            refresh: false,
            failures: 0,
            synced: None,
            corrections: Vec::new(),
            restarts: Vec::new(),
            muted: Vec::new(),
            drift_error: None,
            tests: Tests::default(),
            phase: Phase::Disconnected,
            device: None,
            error: None,
            published: None,
        };
        supervisor.note("engine started");
        supervisor.poll(now);
        supervisor.publish();
        supervisor
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    /// When [`Supervisor::step`] next has something to do.
    pub(super) fn next_wake(&self) -> Instant {
        let free = self.present && !self.tests.holds_the_dongle();
        let work = self.work_at.filter(|_| free);
        let look = (self.refresh && free && self.phase != Phase::Waiting).then(|| self.now());
        [
            Some(self.next_poll),
            work,
            self.save_at,
            look,
            self.tests.next_tick(),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(self.next_poll)
    }

    /// Takes one command. Nothing is written to the dongle here; the next steps do that.
    pub(super) fn command(&mut self, command: Command) {
        let now = self.now();
        let mut next = self.cfg.clone();
        let checked = match command {
            Command::SetMode(mode) => {
                next.mode = Some(mode);
                Ok(())
            }
            Command::SetMinHissMs(ms) => {
                mode::settle_units(ms).map(|_| next.min_hiss_ms = Some(ms))
            }
            Command::SetMicBoost(step) => {
                patches::mic_boost(step).map(|_| next.mic_boost = Some(step))
            }
            Command::SetBalancedDecayMs(ms) => {
                mode::decay_units(ms).map(|_| next.balanced_decay_ms = Some(ms))
            }
            Command::SetNoiseReduction(on) => {
                next.mic_nr = Some(on);
                Ok(())
            }
            Command::SetAgc(on) => {
                next.mic_agc = Some(on);
                Ok(())
            }
            Command::SetSidetone(on) => {
                next.sidetone = Some(on);
                Ok(())
            }
            Command::SetSidetoneLevel(level) => {
                patches::sidetone_level(level).map(|_| next.sidetone_level = Some(level))
            }
            Command::SetSurround(on) => {
                next.surround = Some(on);
                Ok(())
            }
            Command::SetSurroundLevel(level) => {
                patches::surround_level(level).map(|_| next.surround_level = Some(level))
            }
            Command::Test(request) => {
                self.test_request(request);
                return;
            }
            Command::Reapply => {
                if self.present {
                    self.force = true;
                    self.forget_drift();
                    self.schedule(now);
                } else {
                    self.note("re-apply ignored: the dongle is not connected");
                }
                return;
            }
            Command::Refresh => {
                self.refresh = true;
                return;
            }
        };
        if let Err(error) = checked {
            let message = match error {
                Error::Refused(message) => message,
                other => other.to_string(),
            };
            self.emit(Event::Error(Failure {
                kind: ErrorKind::Invalid,
                message,
            }));
            return;
        }
        if next != self.cfg {
            self.cfg = next;
            self.forget_drift();
            self.save_failures = 0;
            let due = now + COALESCE;
            self.save_at = Some(self.save_at.map_or(due, |at| at.min(due)));
        }
        self.schedule(now + COALESCE);
    }

    /// The computer woke up from sleep or hibernation: the dongle may have restarted, so it is
    /// left alone for [`SETTLE`] and then verified.
    pub(super) fn resumed(&mut self) {
        self.note("the computer resumed");
        self.abort_tests("the computer went to sleep", true);
        if self.present {
            let now = self.now();
            self.wait_for_firmware(now);
        }
    }

    /// Does what is due: checks presence, writes the settings file, reads and, if needed,
    /// writes the dongle. Then publishes the state if it changed.
    pub(super) fn step(&mut self) {
        let now = self.now();
        if let Some(took) = self.watch.take_recovered() {
            self.note(format!(
                "the dongle answered again after {} s",
                took.as_secs()
            ));
            self.published = None;
        }
        if now >= self.next_poll {
            self.next_poll = now + POLL;
            self.poll(now);
        }
        if self.save_at.is_some_and(|at| now >= at) {
            self.save();
        }
        if self.present && !self.tests.holds_the_dongle() {
            if self.work_at.is_some_and(|at| now >= at) {
                self.tests.release_dongle();
                self.work();
            } else if self.refresh && self.phase != Phase::Waiting {
                self.tests.release_dongle();
                self.look();
            }
        }
        self.tick_tests(now);
        self.publish();
    }

    /// Writes what is still unsaved. Called when the engine stops.
    pub(super) fn finish(&mut self) {
        self.abort_tests("the engine was stopped", false);
        if self.saved != self.cfg {
            self.save();
        }
        self.note("engine stopped");
    }

    /// The worker thread failed: saves what is unsaved and publishes a last state that says the
    /// engine does nothing any more.
    pub(super) fn stopped(&mut self, failure: Failure) {
        self.note(format!("error ({}): {}", failure.kind, failure.message));
        let saving = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.finish()));
        self.phase = Phase::Stopped;
        self.error = Some(failure.clone());
        self.emit(Event::Error(failure));
        self.publish();
        drop(saving);
    }

    /// Brings the next pass forward to `due` unless one is already due earlier. A dongle that is
    /// still settling keeps its own schedule.
    fn schedule(&mut self, due: Instant) {
        self.failures = 0;
        if self.present && self.phase != Phase::Waiting {
            self.work_at = Some(self.work_at.map_or(due, |at| at.min(due)));
        }
    }

    fn present_now(&mut self) -> crate::Result<bool> {
        let _call = self.watch.enter("listing the connected devices");
        self.backend.present()
    }

    fn poll(&mut self, now: Instant) {
        match self.present_now() {
            Ok(present) => {
                self.enumeration_error = None;
                self.presence(present, now);
            }
            Err(error) => {
                let text = error.to_string();
                if self.enumeration_error.as_ref() != Some(&text) {
                    self.note(format!("enumeration failed: {text}"));
                }
                self.enumeration_error = Some(text);
            }
        }
    }

    fn presence(&mut self, present: bool, now: Instant) {
        if present == self.present {
            return;
        }
        self.present = present;
        if present {
            self.note("the dongle connected");
            self.wait_for_firmware(now);
        } else {
            self.note("the dongle disconnected");
            self.abort_tests("the dongle was disconnected", false);
            self.forget_device();
            self.phase = Phase::Disconnected;
        }
    }

    fn wait_for_firmware(&mut self, now: Instant) {
        self.forget_device();
        self.phase = Phase::Waiting;
        self.work_at = Some(now + SETTLE);
    }

    /// Drops everything that was learned about the dongle in the session that just ended.
    fn forget_device(&mut self) {
        self.device = None;
        self.error = None;
        self.failures = 0;
        self.force = false;
        self.refresh = false;
        self.synced = None;
        self.forget_drift();
    }

    /// Forgets which settings something else keeps changing, so that they are corrected again.
    fn forget_drift(&mut self) {
        self.corrections.clear();
        self.muted.clear();
        self.drift_error = None;
    }

    /// Reads the dongle and, if it differs from the saved settings, writes them.
    fn work(&mut self) {
        let force = std::mem::take(&mut self.force);
        self.refresh = false;
        let result = self.sync(force);
        let now = self.now();
        match result {
            Ok(Pass::Restarted) => {}
            Ok(Pass::Done) => {
                if self.error.take().is_some() {
                    self.note("recovered");
                }
                self.failures = 0;
                self.phase = Phase::Connected;
                self.work_at = Some(now + VERIFY);
            }
            Err(failure) => self.failed(failure, now),
        }
    }

    /// Reads the dongle and publishes what it holds; writes nothing.
    fn look(&mut self) {
        self.refresh = false;
        if let Err(failure) = self.read() {
            let now = self.now();
            self.failed(failure, now);
        }
    }

    fn read(&mut self) -> Result<(Watched<B::Dev>, Readback), Failure> {
        let mut dev = {
            let _call = self.watch.enter("opening the dongle");
            Watched::new(self.backend.open()?, self.watch.clone())
        };
        let state = mode::read_state(&mut dev)?;
        let seen = Readback::of(&state);
        self.device = Some(seen.clone());
        Ok((dev, seen))
    }

    /// A dongle that held the chosen settings at the last pass and now holds the vendor's has
    /// restarted (a USB reset, a power loss) without the poll seeing it go. Nothing is fighting
    /// over it, so this is no drift. Another program is unlikely to reset every managed setting to
    /// the vendor's at once.
    fn restarted(&self, seen: &Readback) -> bool {
        self.synced.as_ref() == Some(&self.cfg)
            && seen.is_vendor_boot()
            && !seen.differences(&self.cfg).is_empty()
    }

    fn sync(&mut self, force: bool) -> Result<Pass, Failure> {
        let (mut dev, mut seen) = self.read()?;
        let now = self.now();
        if !force && self.restarted(&seen) {
            self.note(
                "the dongle holds the vendor settings again: it restarted since the last check",
            );
            self.count_restart(now);
            self.wait_for_firmware(now);
            return Ok(Pass::Restarted);
        }
        self.put_back_deferred_noise_reduction(&mut dev, &mut seen);
        if !force {
            self.follow_surround(&seen, now);
        }
        // a setting that the dongle holds again is corrected again if it drifts again
        let held_back = seen.differences(&self.cfg);
        self.muted.retain(|name| held_back.contains(name));
        if self.muted.is_empty() {
            self.drift_error = None;
        }
        let mut drift = seen.differences(&without(&self.cfg, &self.muted));
        if !force && !drift.is_empty() && self.synced.as_ref() == Some(&self.cfg) {
            drift = self.changed_by_something_else(drift, now);
        }
        if drift.is_empty() && !force {
            self.synced = Some(self.cfg.clone());
            return Ok(Pass::Done);
        }
        self.phase = Phase::Applying;
        self.publish();
        self.note(if drift.is_empty() {
            "applying the saved settings (re-apply requested)".to_string()
        } else {
            format!(
                "applying the saved settings; the dongle differs in: {}",
                drift.join(", ")
            )
        });

        let cfg = without(&self.cfg, &self.muted);
        let watch = self.watch.clone();
        let Attempt { report, error } = settings::ensure(&mut dev, &cfg, || {
            let _call = watch.enter("opening the silent stream");
            self.backend.open_stream()
        });
        self.note(format!("{} writes made", report.writes));
        for event in &report.stream_events {
            self.warn(event.to_string());
        }
        let after = mode::read_state(&mut dev).map(|state| Readback::of(&state));
        if let Ok(after) = &after {
            self.device = Some(after.clone());
        }
        if let Some(error) = error {
            return Err(error.into());
        }
        let left = after?.differences(&cfg);
        if left.is_empty() {
            self.synced = Some(self.cfg.clone());
            Ok(Pass::Done)
        } else {
            Err(Failure {
                kind: ErrorKind::Verify,
                message: format!(
                    "after the saved settings were applied the dongle does not hold: {}",
                    left.join(", ")
                ),
            })
        }
    }

    /// Counts a restart. A dongle that restarts again and again (a failing cable or port, a power
    /// problem) is restored every time, and from [`WARN_RESTARTS`] within [`DRIFT_WINDOW`] each
    /// restart is also reported, so that the cause can be looked for.
    fn count_restart(&mut self, now: Instant) {
        self.restarts
            .retain(|at| now.saturating_duration_since(*at) < DRIFT_WINDOW);
        self.restarts.push(now);
        if self.restarts.len() >= WARN_RESTARTS {
            self.warn(format!(
                "the dongle has restarted {} times in {} minutes (check the cable and the USB \
                 port); the settings are applied again each time",
                self.restarts.len(),
                DRIFT_WINDOW.as_secs() / 60
            ));
        }
    }

    /// The dongle's own control can switch the virtual surround. A switch on a dongle that held
    /// the chosen settings at the last pass is the user's, so the chosen setting follows it
    /// instead of being corrected.
    fn follow_surround(&mut self, seen: &Readback, now: Instant) {
        let Some(chosen) = self.cfg.surround else {
            return;
        };
        if chosen == seen.surround || self.synced.as_ref() != Some(&self.cfg) {
            return;
        }
        self.note(format!(
            "virtual surround was switched {} on the dongle; keeping that",
            if seen.surround { "on" } else { "off" }
        ));
        self.cfg.surround = Some(seen.surround);
        if let Some(synced) = &mut self.synced {
            synced.surround = Some(seen.surround);
        }
        self.save_failures = 0;
        let due = now + COALESCE;
        self.save_at = Some(self.save_at.map_or(due, |at| at.min(due)));
    }

    /// Something changed settings on a dongle that was holding them, and the user did not ask for
    /// it. Each such change is corrected and reported, up to [`MAX_CORRECTIONS`] times within
    /// [`DRIFT_WINDOW`]; after that the setting is left alone and the problem is reported, because
    /// every correction can reload the codec's DSP and another program would be fought for ever.
    /// Returns the settings to correct.
    fn changed_by_something_else(
        &mut self,
        drift: Vec<&'static str>,
        now: Instant,
    ) -> Vec<&'static str> {
        let minutes = DRIFT_WINDOW.as_secs() / 60;
        self.corrections
            .retain(|(_, at)| now.saturating_duration_since(*at) < DRIFT_WINDOW);
        let (mut correct, mut leave) = (Vec::new(), Vec::new());
        for name in drift {
            let times = self.corrections.iter().filter(|(n, _)| *n == name).count();
            if times >= MAX_CORRECTIONS {
                leave.push(name);
            } else {
                self.corrections.push((name, now));
                self.warn(format!(
                    "{name} changed on the dongle without cxtune; correcting it \
                     ({} of {MAX_CORRECTIONS} allowed in {minutes} minutes)",
                    times + 1
                ));
                correct.push(name);
            }
        }
        if !leave.is_empty() {
            self.muted.extend(&leave);
            let failure = Failure {
                kind: ErrorKind::Drift,
                message: format!(
                    "{} keeps changing on the dongle without cxtune (probably another program \
                     that controls it): corrected {MAX_CORRECTIONS} times in {minutes} minutes, \
                     now left alone. Close the other program, or change a setting, re-apply or \
                     replug the dongle to start again",
                    leave.join(", ")
                ),
            };
            if self.drift_error.as_ref() != Some(&failure) {
                self.note(format!("error ({}): {}", failure.kind, failure.message));
                self.emit(Event::Error(failure.clone()));
            }
            self.drift_error = Some(failure);
        }
        correct
    }

    fn failed(&mut self, failure: Failure, now: Instant) {
        // A dongle that went away while it was being used is a disconnect, not an error.
        if failure.kind == ErrorKind::Communication && matches!(self.present_now(), Ok(false)) {
            self.note(format!("the dongle went away: {}", failure.message));
            self.presence(false, now);
            return;
        }
        self.failures += 1;
        self.phase = Phase::Connected;
        self.work_at = retry_delay(failure.kind, self.failures).map(|delay| now + delay);
        match self.work_at {
            Some(at) => self.note(format!(
                "failed ({} in a row), next attempt in {} s",
                self.failures,
                at.saturating_duration_since(now).as_secs()
            )),
            None => self.note("failed; no further attempt until something changes"),
        }
        if self.error.as_ref() != Some(&failure) {
            self.note(format!("error ({}): {}", failure.kind, failure.message));
            self.emit(Event::Error(failure.clone()));
        }
        self.error = Some(failure);
    }

    /// Writes the settings file; if that fails, tries again after a pause that grows with each
    /// failure, for as long as the file does not hold the settings.
    fn save(&mut self) {
        self.save_at = None;
        if self.saved == self.cfg {
            self.save_error = None;
            return;
        }
        match config::save_to(&self.cfg_path, &self.cfg) {
            Ok(()) => {
                self.saved = self.cfg.clone();
                self.save_failures = 0;
                if self.save_error.take().is_some() {
                    self.note("the settings file was written after earlier failures");
                }
                self.note(format!("saved settings: {:?}", self.cfg));
            }
            Err(error) => {
                let failure = Failure::from(error);
                self.save_failures += 1;
                let now = self.now();
                self.save_at =
                    retry_delay(failure.kind, self.save_failures).map(|delay| now + delay);
                if self.save_error.as_ref() != Some(&failure) {
                    self.note(format!("error ({}): {}", failure.kind, failure.message));
                    self.emit(Event::Error(failure.clone()));
                }
                self.save_error = Some(failure);
            }
        }
    }

    fn publish(&mut self) {
        let snapshot = Snapshot {
            phase: self.phase,
            settings: self.cfg.clone(),
            settings_saved: self.saved == self.cfg,
            save_error: self.save_error.clone(),
            device: self.device.clone(),
            error: self.error.clone().or_else(|| self.drift_error.clone()),
            test: self.tests.state(),
        };
        if self.published.as_ref() != Some(&snapshot) {
            self.published = Some(snapshot.clone());
            self.emit(Event::State(snapshot));
        }
    }

    fn emit(&mut self, event: Event) {
        (self.sink)(event);
    }

    fn note(&mut self, message: impl AsRef<str>) {
        (self.log)(message.as_ref());
    }

    fn warn(&mut self, message: String) {
        self.note(format!("warning: {message}"));
        self.emit(Event::Warning(message));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::engine::testing::Rig;
    use crate::fake::FakeDevice;
    use crate::mode::Mode;

    /// Settings that the stock dongle needs a mode change and a mic boost write for.
    fn settings() -> Config {
        Config {
            mode: Some(Mode::Balanced),
            mic_boost: Some(6),
            mic_nr: Some(true),
            mic_agc: Some(false),
            sidetone: Some(false),
            ..Config::default()
        }
    }

    /// A connected dongle that has been brought to [`settings`].
    fn connected() -> Rig {
        let mut rig = Rig::new(settings(), true);
        rig.run(5.0);
        assert_eq!(rig.phase(), Phase::Connected);
        assert!(rig.snapshot().in_sync(), "{:?}", rig.snapshot());
        rig
    }

    fn level_writes(rig: &Rig) -> Vec<u16> {
        rig.dev
            .writes()
            .into_iter()
            .filter(|(addr, _)| *addr == 0x1276)
            .map(|(_, data)| u16::from_le_bytes([data[0], data[1]]))
            .collect()
    }

    fn applying(rig: &Rig) -> usize {
        rig.states()
            .iter()
            .filter(|s| s.phase == Phase::Applying)
            .count()
    }

    #[test]
    fn the_first_state_already_says_whether_the_dongle_is_there() {
        let there = Rig::new(settings(), true);
        assert_eq!(there.states().len(), 1);
        assert_eq!(there.phase(), Phase::Waiting);
        assert_eq!(there.opens(), 0);

        let away = Rig::new(settings(), false);
        assert_eq!(away.states().len(), 1);
        assert_eq!(away.phase(), Phase::Disconnected);
    }

    #[test]
    fn a_dongle_that_appears_is_left_alone_until_its_firmware_has_loaded() {
        let mut rig = Rig::new(settings(), false);
        rig.run(3.0);
        assert_eq!(rig.phase(), Phase::Disconnected);
        assert_eq!(rig.opens(), 0);

        rig.set_present(true);
        rig.run(1.9);
        assert_eq!(rig.phase(), Phase::Waiting);
        assert_eq!((rig.opens(), rig.dev.requests()), (0, 0));

        rig.run(2.2);
        assert_eq!(rig.phase(), Phase::Connected);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.dev.streams_opened(), 1);
    }

    #[test]
    fn a_command_given_while_the_dongle_settles_does_not_cut_the_wait_short() {
        let mut rig = Rig::new(settings(), true);
        rig.run(0.5);
        assert_eq!(rig.phase(), Phase::Waiting);
        rig.send(Command::SetMicBoost(7));
        rig.run(1.0);
        assert_eq!((rig.phase(), rig.opens()), (Phase::Waiting, 0));
        rig.run(1.0);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.dev.byte(0x1017), 0x3F);
    }

    #[test]
    fn what_was_learned_about_a_dongle_is_forgotten_when_it_goes_away() {
        let dev = FakeDevice::stock();
        // an unsupported firmware: a refusal is open and the readback is known
        dev.poke(0x1001, &[0x16]);
        let mut rig = Rig::with_device(dev, settings(), true);
        rig.run(5.0);
        assert!(rig.snapshot().error.is_some() && rig.snapshot().device.is_some());

        rig.set_present(false);
        rig.run(2.0);
        let snapshot = rig.snapshot();
        assert_eq!(
            (snapshot.phase, snapshot.device, snapshot.error),
            (Phase::Disconnected, None, None)
        );
    }

    #[test]
    fn a_dongle_that_goes_away_while_settling_starts_the_wait_again() {
        let mut rig = Rig::new(settings(), false);
        rig.run(1.0);
        rig.set_present(true);
        rig.run(1.0); // seen by the poll at 2 s: settled at 4 s
        assert_eq!(rig.phase(), Phase::Waiting);
        rig.set_present(false);
        rig.run(1.0); // gone at 3 s
        assert_eq!(rig.phase(), Phase::Disconnected);
        rig.set_present(true);
        rig.run(1.0); // back at 4 s: settled at 6 s, not at 4 s
        assert_eq!(rig.phase(), Phase::Waiting);
        rig.run(1.0);
        assert_eq!((rig.phase(), rig.opens()), (Phase::Waiting, 0));
        rig.run(1.2);
        assert_eq!(rig.phase(), Phase::Connected);
        assert!(rig.snapshot().in_sync());
    }

    #[test]
    fn a_cable_that_keeps_failing_causes_no_writes_and_no_errors() {
        let mut rig = Rig::new(settings(), false);
        // every presence is seen by one poll, then it is gone again
        for i in 0..40 {
            rig.set_present(i % 2 == 0);
            rig.run(1.0);
        }
        assert_eq!((rig.opens(), rig.dev.requests()), (0, 0));
        assert!(rig.errors().is_empty() && rig.warnings().is_empty());
        assert_eq!(applying(&rig), 0);
        assert!(rig.log().len() <= 2 * 40 + 2, "{}", rig.log().len());

        rig.set_present(true);
        rig.run(5.0);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.dev.streams_opened(), 1);
        assert_eq!(applying(&rig), 1);
    }

    #[test]
    fn a_dongle_that_restarts_again_and_again_is_restored_once_per_restart() {
        let mut rig = connected();
        let per_restore = rig.dev.writes().len();
        assert!(per_restore > 0);
        for round in 1..=5 {
            rig.set_present(false);
            rig.run(1.5);
            rig.dev.replug();
            rig.set_present(true);
            rig.run(5.0);
            assert!(rig.snapshot().in_sync(), "round {round}");
            assert_eq!(
                rig.dev.writes().len(),
                per_restore * (round + 1),
                "round {round}"
            );
        }
        assert!(rig.errors().is_empty());
        assert_eq!(rig.dev.streams_opened(), 6);
    }

    #[test]
    fn a_dongle_that_is_in_order_is_only_read() {
        let mut rig = connected();
        let (writes, requests, opens) = (rig.dev.writes().len(), rig.dev.requests(), rig.opens());
        rig.run(60.0);
        assert_eq!(rig.dev.writes().len(), writes);
        assert_eq!(rig.dev.streams_opened(), 1);
        assert!(rig.dev.requests() > requests);
        // one reading per VERIFY
        let passes = rig.opens() - opens;
        assert!((3..=5).contains(&passes), "{passes}");
        assert!(rig.errors().is_empty());
        assert_eq!(applying(&rig), 1);
    }

    #[test]
    fn drift_is_found_by_the_periodic_check_and_put_right() {
        let mut rig = connected();
        let writes = rig.dev.writes().len();
        rig.dev.poke(0x1017, &[0x3C]);
        rig.run(VERIFY.as_secs_f64() + 1.0);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.dev.byte(0x1017), 0x3E);
        assert_eq!(rig.dev.writes().len(), writes + 1);
        assert_eq!(rig.dev.streams_opened(), 1);
        assert!(
            rig.log()
                .iter()
                .any(|line| line.ends_with("differs in: mic boost"))
        );
    }

    /// What the dongle's AGC bit does while something else fights cxtune over it.
    fn agc_is_switched_on_behind_the_engine(rig: &Rig) {
        rig.dev.poke(0x1151, &[0x14]);
    }

    fn corrections(rig: &Rig) -> usize {
        rig.warnings()
            .iter()
            .filter(|w| w.contains("changed on the dongle without cxtune"))
            .count()
    }

    fn drift_errors(rig: &Rig) -> Vec<Failure> {
        rig.errors()
            .into_iter()
            .filter(|e| e.kind == ErrorKind::Drift)
            .collect()
    }

    /// Lets the dongle drift and the engine find it, `times` times.
    fn fight(rig: &mut Rig, times: usize) {
        for _ in 0..times {
            agc_is_switched_on_behind_the_engine(rig);
            rig.run(VERIFY.as_secs_f64() + 1.0);
        }
    }

    #[test]
    fn every_correction_of_drift_is_a_warning() {
        let mut rig = connected();
        fight(&mut rig, 1);
        assert!(rig.snapshot().in_sync());
        let warnings = rig.warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("AGC changed on the dongle without cxtune"));
        assert!(warnings[0].contains("1 of 3"), "{}", warnings[0]);
        fight(&mut rig, 1);
        assert!(rig.warnings()[1].contains("2 of 3"));
        assert!(rig.errors().is_empty());
    }

    #[test]
    fn something_that_keeps_changing_a_setting_is_corrected_a_few_times_and_then_left_alone() {
        let mut rig = connected();
        fight(&mut rig, MAX_CORRECTIONS);
        assert_eq!(corrections(&rig), MAX_CORRECTIONS);
        assert!(rig.snapshot().in_sync());
        assert!(rig.errors().is_empty());

        let writes = rig.dev.writes().len();
        fight(&mut rig, 1);
        assert_eq!(rig.dev.writes().len(), writes, "not corrected again");
        assert_eq!(rig.dev.byte(0x1151), 0x14);
        let errors = drift_errors(&rig);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("AGC"), "{}", errors[0].message);
        let state = rig.snapshot();
        assert_eq!(state.error.as_ref(), Some(&errors[0]));
        assert!(
            !state.in_sync(),
            "the snapshot does not claim the dongle holds AGC off"
        );
        assert_eq!(state.phase, Phase::Connected);

        // it stays quiet: no more writes, warnings or errors however long this goes on
        rig.run(600.0);
        assert_eq!(rig.dev.writes().len(), writes);
        assert_eq!(corrections(&rig), MAX_CORRECTIONS);
        assert_eq!(drift_errors(&rig).len(), 1);
        assert_eq!(rig.snapshot().error.as_ref(), Some(&errors[0]));
    }

    #[test]
    fn other_settings_are_still_corrected_while_one_is_left_alone() {
        let mut rig = connected();
        fight(&mut rig, MAX_CORRECTIONS + 1);
        assert_eq!(drift_errors(&rig).len(), 1);

        rig.dev.poke(0x1017, &[0x3C]);
        rig.run(VERIFY.as_secs_f64() + 1.0);
        assert_eq!(rig.dev.byte(0x1017), 0x3E);
        assert_eq!(rig.dev.byte(0x1151), 0x14);
        assert_eq!(drift_errors(&rig).len(), 1);
    }

    #[test]
    fn a_choice_a_re_apply_or_a_replug_starts_the_corrections_again() {
        let leave_agc_alone = || {
            let mut rig = connected();
            fight(&mut rig, MAX_CORRECTIONS + 1);
            assert!(rig.snapshot().error.is_some());
            rig
        };

        // a command (any setting)
        let mut rig = leave_agc_alone();
        rig.send(Command::SetMicBoost(5));
        rig.run(1.0);
        assert!(rig.snapshot().in_sync(), "{:?}", rig.snapshot());
        assert!(rig.snapshot().error.is_none());
        assert_eq!(rig.dev.byte(0x1151), 0x04);
        fight(&mut rig, 1);
        assert_eq!(corrections(&rig), MAX_CORRECTIONS + 1);

        // a re-apply
        let mut rig = leave_agc_alone();
        rig.send(Command::Reapply);
        rig.run(1.0);
        assert!(rig.snapshot().in_sync());
        assert!(rig.snapshot().error.is_none());

        // a replug
        let mut rig = leave_agc_alone();
        rig.set_present(false);
        rig.run(2.0);
        rig.set_present(true);
        rig.dev.replug();
        rig.run(5.0);
        assert!(rig.snapshot().in_sync());
        assert!(rig.snapshot().error.is_none());
        // the corrections of the earlier connection are forgotten
        fight(&mut rig, 1);
        assert!(rig.snapshot().in_sync());
        assert_eq!(corrections(&rig), MAX_CORRECTIONS + 1);
    }

    #[test]
    fn a_setting_that_holds_again_is_no_longer_reported() {
        let mut rig = connected();
        fight(&mut rig, MAX_CORRECTIONS + 1);
        assert!(rig.snapshot().error.is_some());

        // the other program has gone: the dongle holds the setting by itself
        rig.dev.poke(0x1151, &[0x04]);
        rig.run(VERIFY.as_secs_f64() + 1.0);
        assert!(rig.snapshot().in_sync());
        assert!(rig.snapshot().error.is_none());

        // and if it starts again the earlier corrections still count: no new attempt, and the
        // problem is reported again
        let writes = rig.dev.writes().len();
        fight(&mut rig, 1);
        assert_eq!(rig.dev.writes().len(), writes);
        assert_eq!(drift_errors(&rig).len(), 2);
        assert!(rig.snapshot().error.is_some());
    }

    #[test]
    fn corrections_that_are_far_apart_are_never_given_up() {
        let mut rig = connected();
        for _ in 0..2 * MAX_CORRECTIONS {
            agc_is_switched_on_behind_the_engine(&rig);
            rig.run(DRIFT_WINDOW.as_secs_f64() / 2.0 + 20.0);
            assert!(rig.snapshot().in_sync());
        }
        assert_eq!(corrections(&rig), 2 * MAX_CORRECTIONS);
        assert!(drift_errors(&rig).is_empty());
    }

    #[test]
    fn a_dongle_that_was_already_in_order_when_it_appeared_is_guarded_too() {
        let cfg = Config {
            mode: Some(Mode::Balanced),
            mic_boost: Some(4),
            mic_nr: Some(true),
            mic_agc: Some(false),
            ..Config::default()
        };
        let mut rig = Rig::with_device(FakeDevice::in_mode(Mode::Balanced), cfg, true);
        rig.run(5.0);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.dev.writes().len(), 0, "nothing needed writing");
        fight(&mut rig, 1);
        assert!(rig.snapshot().in_sync());
        assert_eq!(corrections(&rig), 1);
    }

    #[test]
    fn changes_the_user_asked_for_and_a_restarted_dongle_are_not_counted_as_drift() {
        let mut rig = connected();
        for step in [1, 2, 3, 4, 5, 7, 6] {
            rig.send(Command::SetMicBoost(step));
            rig.send(Command::SetNoiseReduction(step % 2 == 0));
            rig.run(1.0);
            assert!(rig.snapshot().in_sync());
        }
        rig.set_present(false);
        rig.run(2.0);
        rig.set_present(true);
        rig.dev.replug();
        rig.run(5.0);
        assert!(rig.snapshot().in_sync());
        assert_eq!(corrections(&rig), 0);
        assert!(rig.warnings().is_empty());
    }

    /// Lets virtual time pass in steps of 0.1 s until `done`, for at most `seconds`.
    fn run_until(rig: &mut Rig, what: &str, seconds: f64, done: impl Fn(&Rig) -> bool) {
        for _ in 0..(seconds * 10.0) as usize {
            if done(rig) {
                return;
            }
            rig.run(0.1);
        }
        panic!("{what} did not happen within {seconds} s");
    }

    /// Time enough for a restart to be found by the periodic check and waited out.
    fn restart_time() -> f64 {
        (VERIFY + SETTLE).as_secs_f64() + 1.0
    }

    #[test]
    fn a_restart_between_two_polls_is_found_by_the_periodic_check_and_waited_out_like_an_arrival() {
        let mut rig = connected();
        let writes = rig.dev.writes().len();
        rig.dev.replug();
        run_until(&mut rig, "the restart to be found", 20.0, |rig| {
            rig.phase() == Phase::Waiting
        });
        assert_eq!(rig.snapshot().device, None);
        assert!(
            rig.log()
                .iter()
                .any(|line| line.contains("restarted since the last check"))
        );

        // the firmware gets its time before anything is written
        rig.run(SETTLE.as_secs_f64() - 0.5);
        assert_eq!(rig.phase(), Phase::Waiting);
        assert_eq!(rig.dev.writes().len(), writes);
        rig.run(1.0);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.dev.streams_opened(), 2);
        assert!(rig.dev.writes().len() > writes);

        // it was no drift: no correction was counted and nothing was reported
        assert!(rig.warnings().is_empty());
        assert!(rig.errors().is_empty());
    }

    #[test]
    fn a_dongle_that_restarts_between_polls_again_and_again_is_restored_every_time() {
        let mut rig = connected();
        let per_restore = rig.dev.writes().len();
        assert!(per_restore > 0);
        for round in 1..=2 * MAX_CORRECTIONS {
            rig.dev.replug();
            rig.run(restart_time());
            assert!(rig.snapshot().in_sync(), "round {round}");
            assert_eq!(
                rig.dev.writes().len(),
                per_restore * (round + 1),
                "round {round}"
            );
        }
        assert!(rig.errors().is_empty());
        assert_eq!(rig.snapshot().error, None);
    }

    fn restart_warnings(rig: &Rig) -> Vec<String> {
        rig.warnings()
            .into_iter()
            .filter(|warning| warning.contains("restarted"))
            .collect()
    }

    #[test]
    fn a_dongle_that_keeps_restarting_is_restored_every_time_and_reported_from_the_third_restart() {
        let mut rig = connected();
        for round in 1..=2 * WARN_RESTARTS {
            rig.dev.replug();
            rig.run(restart_time());
            assert!(rig.snapshot().in_sync(), "round {round}");
            let warnings = restart_warnings(&rig);
            assert_eq!(
                warnings.len(),
                round.saturating_sub(WARN_RESTARTS - 1),
                "round {round}: {warnings:?}"
            );
            if let Some(latest) = warnings.last() {
                assert!(
                    latest.contains(&format!("restarted {round} times in 10 minutes")),
                    "{latest}"
                );
                assert!(latest.contains("the USB port); the settings"), "{latest}");
            }
        }
        // a warning does not stop anything, and a restart is no drift
        assert!(rig.errors().is_empty());
        assert_eq!(rig.snapshot().error, None);
        assert_eq!(corrections(&rig), 0);
    }

    #[test]
    fn restarts_far_apart_are_not_reported() {
        let mut rig = connected();
        for _ in 0..2 * WARN_RESTARTS {
            rig.dev.replug();
            rig.run(DRIFT_WINDOW.as_secs_f64() + 20.0);
            assert!(rig.snapshot().in_sync());
        }
        assert!(restart_warnings(&rig).is_empty(), "{:?}", rig.warnings());
    }

    #[test]
    fn a_restart_ends_the_give_up_of_a_setting_that_was_left_alone() {
        let mut rig = connected();
        fight(&mut rig, MAX_CORRECTIONS + 1);
        assert!(rig.snapshot().error.is_some());

        rig.dev.replug();
        rig.run(restart_time());
        assert!(rig.snapshot().in_sync());
        assert!(rig.snapshot().error.is_none());
        assert_eq!(rig.dev.byte(0x1151), 0x04);

        // the corrections of before the restart no longer count
        fight(&mut rig, 1);
        assert!(rig.snapshot().in_sync());
        assert_eq!(corrections(&rig), MAX_CORRECTIONS + 1);
    }

    #[test]
    fn a_dongle_that_only_looks_like_a_restarted_one_is_drift() {
        let mut rig = connected();
        // everything the vendor's state has, except the sidetone level
        rig.dev.replug();
        rig.dev.poke(0x1276, &[0x00, 0x02]);
        rig.run(VERIFY.as_secs_f64() + 0.5);
        assert!(rig.snapshot().in_sync());
        assert_eq!(corrections(&rig), 2, "{:?}", rig.warnings());
        assert!(
            !rig.log()
                .iter()
                .any(|line| line.contains("restarted since the last check"))
        );
    }

    #[test]
    fn a_re_apply_does_not_wait_for_a_restarted_dongle() {
        let mut rig = connected();
        rig.dev.replug();
        rig.send(Command::Reapply);
        rig.run(1.0);
        assert_eq!(rig.phase(), Phase::Connected);
        assert!(rig.snapshot().in_sync());
    }

    #[test]
    fn a_dongle_that_is_meant_to_hold_the_vendor_settings_is_never_taken_for_a_restarted_one() {
        let cfg = Config {
            mode: Some(Mode::Stock),
            mic_boost: Some(4),
            ..Config::default()
        };
        let mut rig = Rig::new(cfg, true);
        rig.run(5.0);
        assert!(rig.snapshot().in_sync());
        rig.run(120.0);
        assert_eq!(rig.phase(), Phase::Connected);
        assert!(rig.log().iter().all(|line| !line.contains("restarted")));
        assert_eq!(rig.dev.writes().len(), 0);
    }

    #[test]
    fn a_burst_of_commands_writes_only_the_last_value() {
        let mut rig = connected();
        for level in 1..=100 {
            rig.send(Command::SetSidetoneLevel(level));
        }
        rig.run(1.0);
        assert_eq!(level_writes(&rig), [100]);
        assert_eq!(rig.saved().sidetone_level, Some(100));
        assert_eq!(rig.snapshot().settings.sidetone_level, Some(100));
        assert!(rig.snapshot().in_sync());
        // one write of the settings file for the whole burst
        let saves = rig
            .log()
            .iter()
            .filter(|l| l.starts_with("saved settings"))
            .count();
        assert_eq!(saves, 1);
    }

    #[test]
    fn commands_spread_over_the_window_save_the_settings_file_once() {
        let mut rig = connected();
        let saves = |rig: &Rig| {
            rig.log()
                .iter()
                .filter(|l| l.starts_with("saved settings"))
                .count()
        };
        let before = saves(&rig);
        for level in 1..=10 {
            rig.send(Command::SetSidetoneLevel(level));
            rig.run(0.01);
        }
        assert_eq!(
            saves(&rig),
            before,
            "the file waits for the end of the window"
        );
        rig.run(1.0);
        assert_eq!(saves(&rig), before + 1);
        assert_eq!(rig.saved().sidetone_level, Some(10));
    }

    #[test]
    fn a_command_that_repeats_a_saved_setting_writes_neither_the_file_nor_the_dongle() {
        let mut rig = connected();
        let writes = rig.dev.writes().len();
        let saved = rig.log().len();
        rig.send(Command::SetMicBoost(6));
        rig.send(Command::SetNoiseReduction(true));
        rig.send(Command::SetMode(Mode::Balanced));
        rig.run(1.0);
        assert_eq!(rig.dev.writes().len(), writes);
        assert!(
            !rig.log()[saved..]
                .iter()
                .any(|line| line.starts_with("saved settings"))
        );
        assert!(!rig.config_path.exists());
    }

    #[test]
    fn a_device_list_that_cannot_be_read_changes_nothing_and_is_logged_once_per_kind() {
        let mut rig = connected();
        let logged = |rig: &Rig| {
            rig.log()
                .iter()
                .filter(|l| l.starts_with("enumeration failed"))
                .count()
        };
        rig.set_enumeration_fails(true);
        rig.run(30.0);
        assert_eq!(rig.phase(), Phase::Connected);
        assert!(rig.snapshot().in_sync());
        assert!(rig.errors().is_empty());
        assert_eq!(logged(&rig), 1);

        // a failure after a success is a new one
        rig.set_enumeration_fails(false);
        rig.run(3.0);
        rig.set_enumeration_fails(true);
        rig.run(3.0);
        assert_eq!(logged(&rig), 2);

        // the dongle going away while the list cannot be read is noticed afterwards
        rig.set_present(false);
        rig.run(3.0);
        assert_eq!(rig.phase(), Phase::Connected);
        rig.set_enumeration_fails(false);
        rig.run(1.5);
        assert_eq!(rig.phase(), Phase::Disconnected);
    }

    #[test]
    fn a_slider_moved_for_a_while_is_written_in_a_bounded_number_of_steps() {
        let mut rig = connected();
        rig.send(Command::SetSidetone(true));
        rig.run(1.0);
        let before = level_writes(&rig).len();

        // 50 commands in half a second, with sidetone on: each change of the level is a ramp
        // of 4 writes
        for step in 1..=50u16 {
            rig.send(Command::SetSidetoneLevel(step * 20));
            rig.run(0.01);
        }
        rig.run(1.0);
        let steps = level_writes(&rig).split_off(before);
        assert_eq!(steps.last(), Some(&1000));
        let windows = (0.5 / COALESCE.as_secs_f64()).ceil() as usize + 1;
        assert!(steps.len() <= windows * 4, "{} writes", steps.len());
        assert!(rig.snapshot().in_sync());
    }

    fn writes_to(rig: &Rig, addr: u16) -> Vec<Vec<u8>> {
        rig.dev
            .writes()
            .into_iter()
            .filter(|(a, _)| *a == addr)
            .map(|(_, data)| data)
            .collect()
    }

    #[test]
    fn the_balanced_decay_time_is_written_in_place_and_saved() {
        let mut rig = connected();
        let streams = rig.dev.streams_opened();
        rig.send(Command::SetBalancedDecayMs(300));
        rig.run(1.0);
        assert_eq!(writes_to(&rig, 0x1F0E), [vec![0x1E]]);
        assert_eq!(rig.saved().balanced_decay_ms, Some(300));
        assert_eq!(rig.snapshot().settings.balanced_decay_ms, Some(300));
        assert_eq!(rig.snapshot().device.unwrap().balanced_decay_ms, Some(300));
        assert!(rig.snapshot().in_sync());
        assert_eq!(
            rig.dev.streams_opened(),
            streams,
            "no stream for an in-place change"
        );
        assert!(rig.errors().is_empty());
    }

    #[test]
    fn a_burst_of_decay_commands_writes_only_the_last_value() {
        let mut rig = connected();
        for i in 0..=190u16 {
            rig.send(Command::SetBalancedDecayMs(100 + 10 * i));
        }
        rig.run(1.0);
        assert_eq!(writes_to(&rig, 0x1F0E), [vec![0xC8]]);
        assert_eq!(rig.saved().balanced_decay_ms, Some(2000));
        assert!(rig.snapshot().in_sync());
    }

    #[test]
    fn the_decay_time_is_remembered_across_other_modes() {
        let mut rig = connected();
        rig.send(Command::SetBalancedDecayMs(300));
        rig.run(1.0);

        // another mode puts the routine's own time back and the decay time is not compared
        rig.send(Command::SetMode(Mode::Quiet));
        rig.run(3.0);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.snapshot().device.unwrap().balanced_decay_ms, None);
        assert_eq!(
            rig.dev.byte(0x1F0E),
            FakeDevice::in_mode(Mode::Quiet).byte(0x1F0E)
        );
        assert_eq!(rig.saved().balanced_decay_ms, Some(300));

        // balanced mode again gets the chosen time
        rig.send(Command::SetMode(Mode::Balanced));
        rig.run(3.0);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.dev.byte(0x1F0E), 0x1E);
        assert_eq!(rig.snapshot().device.unwrap().balanced_decay_ms, Some(300));
    }

    #[test]
    fn a_decay_time_changed_by_something_else_is_put_right() {
        let mut rig = connected();
        rig.send(Command::SetBalancedDecayMs(300));
        rig.run(1.0);
        rig.dev.poke(0x1F0E, &[0x20]);
        rig.run(20.0);
        assert_eq!(rig.dev.byte(0x1F0E), 0x1E);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.warnings().len(), 1, "{:?}", rig.warnings());
    }

    #[test]
    fn the_surround_and_its_level_are_written_and_saved() {
        let mut rig = connected();
        let streams = rig.dev.streams_opened();
        rig.send(Command::SetSurround(true));
        rig.send(Command::SetSurroundLevel(8));
        rig.run(1.0);
        assert_eq!(rig.dev.byte(0x1152), 0x08);
        assert_eq!(rig.dev.byte(0x10D3), 0x08);
        assert_eq!(rig.saved().surround, Some(true));
        assert_eq!(rig.saved().surround_level, Some(8));
        let device = rig.snapshot().device.unwrap();
        assert!(device.surround);
        assert_eq!(device.surround_level, Some(8));
        assert!(rig.snapshot().in_sync());
        assert_eq!(
            rig.dev.streams_opened(),
            streams,
            "no stream for the surround"
        );
        // the choice was not taken for a switch on the dongle and undone, now or later
        rig.run(30.0);
        assert_eq!(rig.snapshot().settings.surround, Some(true));
        assert_eq!(rig.dev.byte(0x1152), 0x08);
        assert!(rig.warnings().is_empty());

        rig.send(Command::SetSurround(false));
        rig.run(1.0);
        assert_eq!(rig.dev.byte(0x1152), 0x00);
        assert_eq!(rig.saved().surround, Some(false));
        assert!(rig.snapshot().in_sync());
    }

    #[test]
    fn a_surround_switch_on_the_dongle_itself_is_kept_and_saved() {
        let mut rig = connected();
        rig.send(Command::SetSurround(false));
        rig.run(1.0);
        let writes = rig.dev.writes().len();

        // the dongle's own control
        rig.dev.poke(0x1152, &[0x08]);
        rig.run(20.0);
        assert_eq!(rig.snapshot().settings.surround, Some(true));
        assert_eq!(rig.saved().surround, Some(true));
        assert_eq!(rig.dev.byte(0x1152), 0x08);
        assert_eq!(rig.dev.writes().len(), writes, "not corrected");
        assert!(rig.warnings().is_empty());
        assert!(rig.errors().is_empty());
        assert!(
            rig.log()
                .iter()
                .any(|l| l.contains("virtual surround was switched on on the dongle"))
        );
        assert!(rig.snapshot().in_sync());

        rig.dev.poke(0x1152, &[0x00]);
        rig.run(20.0);
        assert_eq!(rig.snapshot().settings.surround, Some(false));
        assert_eq!(rig.saved().surround, Some(false));
        assert_eq!(rig.dev.writes().len(), writes);
    }

    #[test]
    fn a_surround_that_is_not_chosen_is_left_as_the_dongle_has_it() {
        let mut rig = connected();
        assert_eq!(rig.snapshot().settings.surround, None);
        rig.dev.poke(0x1152, &[0x08]);
        rig.run(20.0);
        assert_eq!(rig.snapshot().settings.surround, None);
        assert!(writes_to(&rig, 0x1152).is_empty());
        assert!(rig.snapshot().in_sync());
        assert!(rig.snapshot().device.unwrap().surround);
        assert!(rig.errors().is_empty() && rig.warnings().is_empty());
    }

    #[test]
    fn a_restarted_dongle_gets_the_chosen_surround_again_and_it_is_no_switch_on_the_dongle() {
        let mut rig = connected();
        rig.send(Command::SetSurround(true));
        rig.run(1.0);
        assert_eq!(rig.dev.byte(0x1152), 0x08);

        rig.dev.replug();
        assert_eq!(rig.dev.byte(0x1152), 0x00);
        run_until(&mut rig, "the restart to be found", 20.0, |rig| {
            rig.phase() == Phase::Waiting
        });
        assert_eq!(rig.snapshot().settings.surround, Some(true));
        rig.run(SETTLE.as_secs_f64() + 1.0);
        assert_eq!(rig.dev.byte(0x1152), 0x08);
        assert_eq!(rig.snapshot().settings.surround, Some(true));
        assert!(rig.snapshot().in_sync());
        assert!(rig.warnings().is_empty());
    }

    #[test]
    fn commands_that_cancel_out_write_nothing() {
        let mut rig = connected();
        let writes = rig.dev.writes().len();
        rig.send(Command::SetMode(Mode::Quiet));
        rig.send(Command::SetMicBoost(2));
        rig.send(Command::SetMode(Mode::Balanced));
        rig.send(Command::SetMicBoost(6));
        rig.run(1.0);
        assert_eq!(rig.dev.writes().len(), writes);
        assert_eq!(rig.dev.streams_opened(), 1);
    }

    #[test]
    fn a_command_is_written_after_the_window_and_not_before() {
        let mut rig = connected();
        rig.send(Command::SetMicBoost(7));
        rig.run(COALESCE.as_secs_f64() - 0.05);
        assert_eq!(rig.dev.byte(0x1017), 0x3E);
        rig.run(0.1);
        assert_eq!(rig.dev.byte(0x1017), 0x3F);
    }

    #[test]
    fn settings_chosen_while_the_dongle_is_away_are_saved_and_applied_when_it_returns() {
        let mut rig = Rig::new(Config::default(), false);
        rig.send(Command::SetMode(Mode::Quiet));
        rig.send(Command::SetMicBoost(7));
        rig.send(Command::SetNoiseReduction(false));
        rig.run(3.0);
        assert_eq!(rig.opens(), 0);
        let saved = rig.saved();
        assert_eq!(
            (saved.mode, saved.mic_boost, saved.mic_nr),
            (Some(Mode::Quiet), Some(7), Some(false))
        );

        rig.set_present(true);
        rig.run(6.0);
        let device = rig.snapshot().device.unwrap();
        assert_eq!(
            (device.mode, device.mic_boost, device.noise_reduction),
            (Some(Mode::Quiet), Some(7), false)
        );
        assert!(rig.snapshot().in_sync());
    }

    #[test]
    fn values_out_of_range_are_rejected_and_change_nothing() {
        let mut rig = connected();
        let opens = rig.opens();
        rig.clear_events();
        let bad = [
            Command::SetMicBoost(8),
            Command::SetSidetoneLevel(0),
            Command::SetSidetoneLevel(4097),
            Command::SetMinHissMs(15),
            Command::SetMinHissMs(10),
            Command::SetMinHissMs(2010),
            Command::SetBalancedDecayMs(0),
            Command::SetBalancedDecayMs(90),
            Command::SetBalancedDecayMs(805),
            Command::SetBalancedDecayMs(2010),
            Command::SetSurroundLevel(0),
            Command::SetSurroundLevel(11),
        ];
        for command in bad {
            rig.send(command);
        }
        rig.run(1.0);
        let errors = rig.errors();
        assert_eq!(errors.len(), bad.len());
        assert!(errors.iter().all(|e| e.kind == ErrorKind::Invalid));
        assert!(
            errors[0].message.contains("mic boost 8"),
            "{}",
            errors[0].message
        );
        // nothing changed: no new state, no file, no pass over the dongle
        assert!(rig.states().is_empty());
        assert!(!rig.config_path.exists());
        assert_eq!(rig.opens(), opens);
    }

    #[test]
    fn a_settings_file_that_cannot_be_written_is_reported_and_the_dongle_still_gets_the_settings() {
        let mut rig = connected();
        // a directory where the file belongs
        std::fs::create_dir_all(&rig.config_path).unwrap();
        rig.send(Command::SetMicBoost(7));
        rig.run(1.0);
        let errors = rig.errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::Config);
        assert!(
            errors[0].message.contains("config.toml"),
            "{}",
            errors[0].message
        );
        assert_eq!(rig.dev.byte(0x1017), 0x3F);
    }

    fn config_errors(rig: &Rig) -> usize {
        rig.errors()
            .iter()
            .filter(|e| e.kind == ErrorKind::Config)
            .count()
    }

    #[test]
    fn a_settings_file_that_cannot_be_written_is_pending_in_every_state_until_it_is_written() {
        let mut rig = connected();
        assert!(rig.snapshot().settings_saved);
        std::fs::create_dir_all(&rig.config_path).unwrap();
        rig.send(Command::SetMicBoost(7));
        rig.sup.step();
        assert!(!rig.snapshot().settings_saved, "pending before the write");
        rig.run(1.0);

        let state = rig.snapshot();
        assert_eq!(state.settings.mic_boost, Some(7));
        assert!(!state.settings_saved);
        assert_eq!(
            state.save_error.as_ref().map(|e| e.kind),
            Some(ErrorKind::Config)
        );
        assert!(state.error.is_none(), "the dongle has no problem");

        // the obstacle goes away between two attempts: the first retry is 2 s after the failure
        std::fs::remove_dir(&rig.config_path).unwrap();
        rig.run(0.9);
        assert!(!rig.snapshot().settings_saved);
        rig.run(0.5);
        let state = rig.snapshot();
        assert!(state.settings_saved);
        assert!(state.save_error.is_none());
        assert_eq!(rig.saved().mic_boost, Some(7));
        // one error event for the whole time, and a note when it was over
        assert_eq!(config_errors(&rig), 1);
        assert!(
            rig.log()
                .iter()
                .any(|l| l.contains("written after earlier failures"))
        );
    }

    #[test]
    fn a_settings_file_that_keeps_failing_is_tried_again_with_growing_pauses_and_reported_once() {
        let mut rig = connected();
        std::fs::create_dir_all(&rig.config_path).unwrap();
        rig.send(Command::SetMicBoost(7));
        rig.run(200.0);
        assert_eq!(config_errors(&rig), 1);
        assert!(!rig.snapshot().settings_saved);

        // the pauses grow to 30 s: a file freed at any time is written within 30 s
        std::fs::remove_dir(&rig.config_path).unwrap();
        rig.run(31.0);
        assert!(rig.snapshot().settings_saved);
        assert_eq!(rig.saved().mic_boost, Some(7));
    }

    #[test]
    fn the_pauses_between_attempts_to_write_the_settings_file_grow() {
        let mut rig = connected();
        std::fs::create_dir_all(&rig.config_path).unwrap();
        rig.send(Command::SetMicBoost(7));
        // attempts at 0.15 s, 2.15 s (2 s pause) and 6.15 s (4 s pause)
        rig.run(3.0);
        std::fs::remove_dir(&rig.config_path).unwrap();
        rig.run(2.0);
        assert!(
            !rig.snapshot().settings_saved,
            "the next attempt is at 6.15 s"
        );
        rig.run(1.5);
        assert!(rig.snapshot().settings_saved);
    }

    #[test]
    fn a_new_change_starts_the_pauses_over() {
        let mut rig = connected();
        std::fs::create_dir_all(&rig.config_path).unwrap();
        rig.send(Command::SetMicBoost(7));
        // failures at 0.15 s, 2.15 s and 6.15 s; the next would be at 14.15 s
        rig.run(7.0);
        rig.send(Command::SetMicBoost(5));
        // the change is tried at 7.15 s and fails; the pause is 2 s again
        rig.run(1.0);
        std::fs::remove_dir(&rig.config_path).unwrap();
        rig.run(1.5);
        assert!(rig.snapshot().settings_saved);
        assert_eq!(rig.saved().mic_boost, Some(5));
    }

    #[test]
    fn a_change_made_while_the_file_cannot_be_written_brings_the_next_attempt_forward() {
        let mut rig = connected();
        std::fs::create_dir_all(&rig.config_path).unwrap();
        rig.send(Command::SetMicBoost(7));
        rig.run(1.0);
        std::fs::remove_dir(&rig.config_path).unwrap();
        rig.send(Command::SetMicBoost(5));
        rig.run(0.3);
        assert!(rig.snapshot().settings_saved);
        assert_eq!(rig.saved().mic_boost, Some(5));
    }

    #[test]
    fn a_setting_changed_back_while_the_file_cannot_be_written_ends_the_problem() {
        let mut rig = connected();
        std::fs::create_dir_all(&rig.config_path).unwrap();
        rig.send(Command::SetMicBoost(7));
        rig.run(1.0);
        assert!(rig.snapshot().save_error.is_some());

        // the file already holds boost 6: there is nothing left to write
        rig.send(Command::SetMicBoost(6));
        rig.run(1.0);
        let state = rig.snapshot();
        assert!(state.settings_saved);
        assert!(state.save_error.is_none());
    }

    #[test]
    fn the_settings_file_is_tried_once_more_when_the_engine_stops() {
        let mut rig = Rig::new(Config::default(), false);
        std::fs::create_dir_all(&rig.config_path).unwrap();
        rig.send(Command::SetMicBoost(3));
        rig.run(1.0);
        assert!(!rig.snapshot().settings_saved);

        // freed after the last attempt and before the next one
        std::fs::remove_dir(&rig.config_path).unwrap();
        rig.sup.finish();
        assert_eq!(rig.saved().mic_boost, Some(3));
    }

    #[test]
    fn a_setting_that_is_changed_back_before_the_write_leaves_the_file_alone() {
        let mut rig = connected();
        rig.send(Command::SetMicBoost(7));
        rig.sup.step();
        assert!(!rig.snapshot().settings_saved);
        rig.send(Command::SetMicBoost(6));
        rig.run(1.0);
        assert!(rig.snapshot().settings_saved);
        assert!(!rig.config_path.exists());
        assert_eq!(config_errors(&rig), 0);
    }

    #[test]
    fn the_unsaved_settings_are_written_when_the_engine_stops() {
        let mut rig = Rig::new(Config::default(), false);
        rig.send(Command::SetMicBoost(3));
        assert!(!rig.config_path.exists());
        rig.sup.finish();
        assert_eq!(rig.saved().mic_boost, Some(3));
        assert_eq!(rig.log().last().map(String::as_str), Some("engine stopped"));
    }

    #[test]
    fn a_dongle_that_is_there_but_does_not_answer_is_retried_with_growing_pauses_and_reported_once()
    {
        let mut rig = Rig::new(settings(), true);
        rig.dev.unplug();
        rig.run(2.5);
        assert_eq!(rig.opens(), 1);
        rig.run(60.0);
        // pauses of 2, 4, 8, 16 and 30 s
        let attempts = rig.opens();
        assert!((5..=8).contains(&attempts), "{attempts}");
        let errors = rig.errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::Communication);
        assert_eq!(rig.snapshot().error.as_ref(), Some(&errors[0]));
        assert_eq!(rig.phase(), Phase::Connected);

        rig.dev.plug_in();
        rig.run(31.0);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.snapshot().error, None);
        assert_eq!(rig.errors().len(), 1);
    }

    #[test]
    fn a_failure_after_a_recovery_starts_again_with_the_short_pauses() {
        let mut rig = Rig::new(settings(), true);
        rig.dev.unplug();
        rig.run(20.0);
        rig.dev.plug_in();
        rig.run(31.0);
        assert!(rig.snapshot().in_sync());

        // the next reading fails: the pauses are 2 and 4 s again, not the 30 s the earlier run
        // of failures had reached, and the new failure is reported
        rig.dev.unplug();
        let before = rig.opens();
        rig.run(VERIFY.as_secs_f64() + 0.5 + 2.1 + 4.1);
        assert_eq!(rig.opens() - before, 3);
        assert_eq!(rig.errors().len(), 2);
    }

    #[test]
    fn a_dongle_that_vanishes_during_a_pass_is_a_disconnect_and_not_an_error() {
        let mut rig = Rig::new(settings(), true);
        rig.vanish_on_open.store(true, Ordering::SeqCst);
        rig.run(4.0);
        assert_eq!(rig.phase(), Phase::Disconnected);
        assert!(rig.errors().is_empty());

        rig.vanish_on_open.store(false, Ordering::SeqCst);
        rig.dev.plug_in();
        rig.dev.replug();
        rig.set_present(true);
        rig.run(6.0);
        assert!(rig.snapshot().in_sync());
    }

    #[test]
    fn a_change_cut_short_by_the_cable_is_completed_when_the_dongle_returns() {
        let mut rig = Rig::new(settings(), true);
        let (dev, present) = (rig.dev.clone(), rig.present.clone());
        // the cable fails after the first three writes of the mode change
        rig.on_stream(move || {
            present.store(false, Ordering::SeqCst);
            dev.fail_after_writes(3);
        });
        rig.run(4.0);
        assert_eq!(rig.phase(), Phase::Disconnected);
        assert!(rig.errors().is_empty());
        assert_eq!(rig.dev.writes().len(), 3);

        rig.on_stream(|| {});
        rig.dev.clear_failure();
        rig.set_present(true);
        rig.run(6.0);
        assert!(rig.snapshot().in_sync());
        let mut expected = FakeDevice::in_mode(Mode::Balanced).mem();
        expected[0x1017] = 0x3E;
        assert_eq!(rig.dev.mem(), expected);
        assert!(rig.errors().is_empty());
    }

    #[test]
    fn a_refusal_keeps_the_report_of_what_was_done_and_is_not_retried_for_ever() {
        let dev = FakeDevice::stock();
        let _mic = dev.open_mic();
        dev.flip_on_newc(0x10);
        // noise reduction and AGC are applied, then the sidetone level is refused, and the
        // mode and the mic boost are still applied
        dev.poke(0x1276, &0x8000u16.to_le_bytes());
        let cfg = Config {
            mic_nr: Some(false),
            mic_agc: Some(true),
            sidetone_level: Some(0x0200),
            ..settings()
        };
        let mut rig = Rig::with_device(dev, cfg, true);
        rig.run(6.0);

        assert_eq!(rig.warnings().len(), 2);
        let errors = rig.errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::Refused);
        assert!(errors[0].message.contains("$1276"), "{}", errors[0].message);
        let snapshot = rig.snapshot();
        let device = snapshot.device.unwrap();
        assert_eq!(device.mode, Some(Mode::Balanced));
        assert_eq!(device.mic_boost, Some(6));
        assert!(!device.noise_reduction && device.agc);
        assert_eq!(snapshot.error.as_ref(), Some(&errors[0]));
        assert_eq!(snapshot.phase, Phase::Connected);

        // looks again after 5, 15 and 60 s, then stops
        rig.run(300.0);
        assert_eq!(rig.opens(), 4);
        assert_eq!(rig.errors().len(), 1);
        assert_eq!(rig.warnings().len(), 2);

        // a refresh reads without starting over; a re-apply starts over
        rig.send(Command::Refresh);
        rig.run(1.0);
        assert_eq!(rig.opens(), 5);
        rig.run(300.0);
        assert_eq!(rig.opens(), 5);
        rig.send(Command::Reapply);
        rig.run(1.0);
        assert_eq!(rig.opens(), 6);
        rig.run(6.0);
        assert_eq!(rig.opens(), 7);
        assert_eq!(rig.errors().len(), 1);
    }

    #[test]
    fn a_dongle_that_stops_holding_the_settings_right_after_they_were_applied_is_a_verify_failure()
    {
        // how many reports a pass over a stock dongle sends
        let mut probe = Rig::new(settings(), true);
        probe.run(5.0);
        let pass = probe.dev.requests();

        // the same pass, with the dongle restarting at the start of the final reading (16 reports)
        let mut rig = Rig::new(settings(), true);
        rig.dev.replug_at_request(pass - 15);
        rig.run(3.0);
        let errors = rig.errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::Verify);
        assert!(
            errors[0].message.contains("does not hold: mode, mic boost"),
            "{}",
            errors[0].message
        );
        assert_eq!(rig.snapshot().error.as_ref(), Some(&errors[0]));
        assert!(!rig.snapshot().in_sync());

        // the next attempt, 10 s later, finds the stock patch and applies the settings again
        rig.run(10.0);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.snapshot().error, None);
        assert_eq!(rig.dev.streams_opened(), 2);
    }

    #[test]
    fn a_write_that_does_not_stick_is_a_verify_failure_that_is_tried_twice_more() {
        let dev = FakeDevice::stock();
        dev.ignore_writes();
        let mut rig = Rig::with_device(dev, settings(), true);
        rig.run(200.0);
        let errors = rig.errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::Verify);
        assert!(
            errors[0].message.contains("READBACK MISMATCH"),
            "{}",
            errors[0].message
        );
        // after 10 and 60 s
        assert_eq!(rig.opens(), 3);
    }

    #[test]
    fn an_unsupported_firmware_is_reported_and_nothing_is_written() {
        let dev = FakeDevice::stock();
        dev.poke(0x1001, &[0x16]);
        let mut rig = Rig::with_device(dev.clone(), settings(), true);
        rig.run(10.0);
        let snapshot = rig.snapshot();
        assert!(!snapshot.device.unwrap().firmware_supported);
        assert_eq!(snapshot.error.unwrap().kind, ErrorKind::Refused);
        assert!(rig.dev.writes().is_empty());
        assert_eq!(rig.dev.streams_opened(), 0);

        // with nothing saved there is nothing to refuse
        let mut rig = Rig::with_device(dev, Config::default(), true);
        rig.run(10.0);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.snapshot().error, None);
    }

    #[test]
    fn after_the_computer_wakes_the_dongle_is_left_alone_and_then_verified() {
        let mut rig = connected();
        rig.dev.replug();
        let requests = rig.dev.requests();
        rig.sup.resumed();
        rig.run(1.9);
        assert_eq!(rig.phase(), Phase::Waiting);
        assert_eq!(rig.dev.requests(), requests);
        assert_eq!(rig.snapshot().device, None);

        rig.run(0.2);
        assert_eq!(rig.phase(), Phase::Connected);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.dev.streams_opened(), 2);
    }

    #[test]
    fn waking_with_the_dongle_away_changes_nothing() {
        let mut rig = Rig::new(settings(), false);
        rig.run(2.0);
        let before = rig.states().len();
        rig.sup.resumed();
        rig.run(3.0);
        assert_eq!(rig.phase(), Phase::Disconnected);
        assert_eq!(rig.states().len(), before);
        assert_eq!(rig.opens(), 0);
    }

    #[test]
    fn a_refresh_reads_without_writing_and_a_reapply_writes_what_differs() {
        let mut rig = connected();
        let writes = rig.dev.writes().len();
        rig.dev.poke(0x1017, &[0x3C]);
        rig.send(Command::Refresh);
        rig.run(0.5);
        assert_eq!(rig.dev.writes().len(), writes);
        let snapshot = rig.snapshot();
        assert_eq!(snapshot.device.as_ref().unwrap().mic_boost, Some(4));
        assert!(!snapshot.in_sync());
        assert_eq!(snapshot.phase, Phase::Connected);

        rig.send(Command::Reapply);
        rig.run(0.5);
        assert!(rig.snapshot().in_sync());
        assert_eq!(rig.dev.writes().len(), writes + 1);
    }

    #[test]
    fn a_reapply_runs_even_when_nothing_seems_to_differ() {
        let mut rig = connected();
        let (opens, writes) = (rig.opens(), rig.dev.writes().len());
        rig.send(Command::Reapply);
        rig.run(0.5);
        assert_eq!(rig.opens(), opens + 1);
        assert_eq!(rig.dev.writes().len(), writes);
        assert!(rig.log().iter().any(|l| l.contains("re-apply requested")));
    }

    #[test]
    fn requests_that_need_the_dongle_wait_for_it() {
        let mut rig = Rig::new(settings(), false);
        rig.send(Command::Reapply);
        rig.send(Command::Refresh);
        rig.run(3.0);
        assert_eq!(rig.opens(), 0);
        assert!(rig.log().iter().any(|l| l.contains("re-apply ignored")));

        // a dongle that is still loading its firmware is not read either
        rig.set_present(true);
        rig.run(1.5);
        rig.send(Command::Refresh);
        rig.run(0.4);
        assert_eq!(rig.phase(), Phase::Waiting);
        assert_eq!(rig.opens(), 0);
        rig.run(1.2);
        assert!(rig.snapshot().in_sync());
    }

    #[test]
    fn the_state_holds_the_saved_settings_and_every_managed_readback() {
        let rig = connected();
        let snapshot = rig.snapshot();
        assert_eq!(snapshot.settings, settings());
        let device = snapshot.device.unwrap();
        assert!(device.firmware_supported);
        assert_eq!(device.mode, Some(Mode::Balanced));
        assert_eq!(device.mic_boost, Some(6));
        assert!(device.noise_reduction && !device.agc && !device.sidetone);
        assert_eq!(device.sidetone_level, 947);
        assert_eq!(device.balanced_decay_ms, Some(800));
        assert!(!device.surround);
        assert_eq!(device.surround_level, Some(4));
        assert!(
            rig.log()
                .iter()
                .any(|l| l.ends_with("differs in: mode, mic boost"))
        );
    }

    #[test]
    fn a_state_is_only_published_when_it_changes() {
        let mut rig = connected();
        rig.run(120.0);
        rig.send(Command::SetSidetoneLevel(500));
        rig.send(Command::SetSidetoneLevel(500));
        rig.run(30.0);
        let states = rig.states();
        for pair in states.windows(2) {
            assert_ne!(pair[0], pair[1]);
        }
    }

    #[test]
    fn retries_follow_the_schedule_of_the_kind_of_failure() {
        let secs = |kind, n| retry_delay(kind, n).map(|d| d.as_secs());
        let run = |kind, n: std::ops::RangeInclusive<u32>| -> Vec<Option<u64>> {
            n.map(|i| secs(kind, i)).collect()
        };
        assert_eq!(
            run(ErrorKind::Communication, 1..=7),
            [2, 4, 8, 16, 30, 30, 30].map(Some)
        );
        assert_eq!(
            run(ErrorKind::Audio, 1..=6),
            [5, 10, 20, 40, 60, 60].map(Some)
        );
        assert_eq!(
            run(ErrorKind::Refused, 1..=4),
            [Some(5), Some(15), Some(60), None]
        );
        assert_eq!(run(ErrorKind::Verify, 1..=3), [Some(10), Some(60), None]);
        assert_eq!(
            run(ErrorKind::Config, 1..=6),
            [2, 4, 8, 16, 30, 30].map(Some)
        );
        for kind in [
            ErrorKind::Invalid,
            ErrorKind::Drift,
            ErrorKind::Unresponsive,
            ErrorKind::Fatal,
        ] {
            assert_eq!(secs(kind, 1), None, "{kind}");
        }
        // a long run of failures does not overflow
        assert_eq!(secs(ErrorKind::Communication, u32::MAX), Some(30));
        assert_eq!(secs(ErrorKind::Audio, 1000), Some(60));
        assert_eq!(secs(ErrorKind::Config, 1000), Some(30));
    }
}
