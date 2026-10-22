//! What the status area says, from the engine's state: one line for what is wrong or how the
//! dongle stands, and one line for what to do about it. Only the most important problem is
//! shown; its technical details are in the log.

use cxtune::Error;
use cxtune::engine::{ErrorKind, Phase, Snapshot};

use crate::ui::{Hint, Title, Tone};

/// What can be wrong, from the most to the least specific cause that the engine reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    Communication,
    Audio,
    Refused,
    UnsupportedFirmware,
    Verify,
    Drift,
    Unresponsive,
    Fatal,
    Busy,
    BadSettingsFile,
    StartFailed,
    Other,
}

impl Problem {
    /// What is wrong, and what to do about it.
    fn lines(self) -> (Title, Hint) {
        match self {
            Problem::Communication => (Title::Communication, Hint::CheckCable),
            Problem::Audio => (Title::Audio, Hint::CloseAudioUsers),
            Problem::Refused => (Title::Refused, Hint::ReplugDongle),
            Problem::UnsupportedFirmware => (Title::UnsupportedFirmware, Hint::LeaveAlone),
            Problem::Verify => (Title::Verify, Hint::ReapplyOrReplug),
            Problem::Drift => (Title::Drift, Hint::CloseIt),
            Problem::Unresponsive => (Title::Unresponsive, Hint::ReplugIt),
            Problem::Fatal => (Title::Fatal, Hint::ReapplyOrRestart),
            Problem::Busy => (Title::Busy, Hint::CloseIt),
            Problem::BadSettingsFile => (Title::BadSettingsFile, Hint::FixFile),
            Problem::StartFailed => (Title::StartFailed, Hint::ReapplyRetry),
            Problem::Other => (Title::Other, Hint::ReapplyRetry),
        }
    }
}

/// The problem that stopped the engine from starting. A refused device lock while a worker of
/// this program is stuck in a call to the dongle (`worker_left_behind`) is that call, not
/// another program: the dongle does not answer.
pub fn start_problem(error: &Error, worker_left_behind: bool) -> Problem {
    match error {
        Error::Busy(_) if worker_left_behind => Problem::Unresponsive,
        Error::Busy(_) => Problem::Busy,
        Error::Config(_) => Problem::BadSettingsFile,
        _ => Problem::StartFailed,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Status {
    pub title: Title,
    pub hint: Hint,
    pub tone: Tone,
    /// Whether the log holds technical details of what the status says. The window never shows
    /// them.
    pub details: bool,
    /// Whether there is anything for Re-apply to do: restart the engine, or apply to a dongle.
    pub reapply: bool,
    /// Whether the engine is there to take the user's choices.
    pub controls: bool,
}

fn problem_of(kind: ErrorKind) -> Problem {
    match kind {
        ErrorKind::Communication => Problem::Communication,
        ErrorKind::Audio => Problem::Audio,
        ErrorKind::Refused => Problem::Refused,
        ErrorKind::Verify => Problem::Verify,
        ErrorKind::Drift => Problem::Drift,
        ErrorKind::Unresponsive => Problem::Unresponsive,
        ErrorKind::Fatal => Problem::Fatal,
        ErrorKind::Config | ErrorKind::Invalid => Problem::Other,
    }
}

/// The status for the latest state of the engine, or for an engine that could not be started.
/// `warned` is whether the engine reported something unusual that was not acknowledged yet.
pub fn status(snapshot: Option<&Snapshot>, start: Option<Problem>, warned: bool) -> Status {
    if let Some(problem) = start {
        let (title, hint) = problem.lines();
        return Status {
            title,
            hint,
            tone: Tone::Critical,
            details: true,
            reapply: true,
            controls: false,
        };
    }
    let Some(snapshot) = snapshot else {
        return Status {
            title: Title::Starting,
            hint: Hint::None,
            tone: Tone::Caution,
            details: false,
            reapply: false,
            controls: false,
        };
    };

    let unsupported = snapshot
        .device
        .as_ref()
        .is_some_and(|device| !device.firmware_supported);
    let state = match snapshot.phase {
        Phase::Stopped => Title::Stopped,
        Phase::Disconnected => Title::Disconnected,
        Phase::Waiting => Title::Waiting,
        Phase::Applying => Title::Applying,
        Phase::Connected if snapshot.device.is_none() => Title::Reading,
        Phase::Connected if unsupported || !snapshot.in_sync() => Title::NotApplied,
        Phase::Connected => Title::InSync,
    };
    // an unsupported firmware is what the engine's refusal is about
    let problem = match &snapshot.error {
        Some(failure) if !(unsupported && failure.kind == ErrorKind::Refused) => {
            Some(problem_of(failure.kind))
        }
        _ if unsupported => Some(Problem::UnsupportedFirmware),
        _ => None,
    };
    let save_failed = snapshot.save_error.is_some();
    let (title, hint) = match problem {
        Some(problem) => problem.lines(),
        None if save_failed => (Title::SaveFailed, Hint::CheckFile),
        None if warned => (state, Hint::SeeLog),
        None if snapshot.settings_saved => (state, Hint::Saved),
        None => (state, Hint::Saving),
    };
    let tone = if problem.is_some() {
        Tone::Critical
    } else if save_failed || warned || state != Title::InSync {
        Tone::Caution
    } else {
        Tone::Good
    };
    Status {
        title,
        hint,
        tone,
        details: snapshot.error.is_some() || save_failed,
        reapply: snapshot.phase != Phase::Disconnected,
        controls: snapshot.phase != Phase::Stopped,
    }
}

#[cfg(test)]
mod tests {
    use cxtune::config::Config;
    use cxtune::engine::{Failure, Readback};
    use cxtune::mode::Mode;

    use super::*;

    fn held(mode: Mode) -> Readback {
        Readback {
            firmware_supported: true,
            mode: Some(mode),
            min_hiss_ms: None,
            mic_boost: Some(4),
            noise_reduction: true,
            agc: false,
            sidetone: false,
            sidetone_level: 947,
        }
    }

    fn snapshot(phase: Phase, device: Option<Readback>) -> Snapshot {
        Snapshot {
            phase,
            settings: Config {
                mode: Some(Mode::Balanced),
                ..Config::default()
            },
            settings_saved: true,
            save_error: None,
            device,
            error: None,
        }
    }

    fn failure(kind: ErrorKind) -> Failure {
        Failure {
            kind,
            message: format!("{kind} happened"),
        }
    }

    fn of(snapshot: &Snapshot) -> Status {
        status(Some(snapshot), None, false)
    }

    #[test]
    fn each_phase_has_its_own_title() {
        let title = |phase, device| of(&snapshot(phase, device)).title;
        assert_eq!(title(Phase::Disconnected, None), Title::Disconnected);
        assert_eq!(title(Phase::Waiting, None), Title::Waiting);
        assert_eq!(
            title(Phase::Applying, Some(held(Mode::Stock))),
            Title::Applying
        );
        assert_eq!(title(Phase::Connected, None), Title::Reading);
        assert_eq!(title(Phase::Stopped, None), Title::Stopped);
    }

    #[test]
    fn a_connected_dongle_is_in_sync_only_when_it_holds_the_chosen_settings() {
        let balanced = of(&snapshot(Phase::Connected, Some(held(Mode::Balanced))));
        assert_eq!((balanced.title, balanced.tone), (Title::InSync, Tone::Good));
        assert_eq!(balanced.hint, Hint::Saved);

        let quiet = of(&snapshot(Phase::Connected, Some(held(Mode::Quiet))));
        assert_eq!(
            (quiet.title, quiet.tone),
            (Title::NotApplied, Tone::Caution)
        );
    }

    #[test]
    fn before_the_first_state_the_engine_is_starting() {
        let status = status(None, None, false);
        assert_eq!(status.title, Title::Starting);
        assert_eq!(status.hint, Hint::None);
        assert!(!status.reapply);
        assert!(!status.controls);
    }

    #[test]
    fn an_engine_that_could_not_start_says_why_and_can_be_started_again() {
        for (error, title, hint) in [
            (Error::Busy("held".into()), Title::Busy, Hint::CloseIt),
            (
                Error::Config("bad".into()),
                Title::BadSettingsFile,
                Hint::FixFile,
            ),
            (
                Error::Device("no hid".into()),
                Title::StartFailed,
                Hint::ReapplyRetry,
            ),
        ] {
            let status = status(None, Some(start_problem(&error, false)), false);
            assert_eq!((status.title, status.hint), (title, hint), "{error}");
            assert_eq!(status.tone, Tone::Critical);
            assert!(status.details && status.reapply && !status.controls);
        }
    }

    #[test]
    fn a_refused_lock_that_a_stuck_worker_of_this_program_holds_asks_for_a_replug() {
        let busy = Error::Busy("previous engine".into());
        let status = status(None, Some(start_problem(&busy, true)), false);
        assert_eq!(
            (status.title, status.hint),
            (Title::Unresponsive, Hint::ReplugIt)
        );
        // only a lock problem is explained by the stuck worker
        let other = Error::Config("bad".into());
        assert_eq!(start_problem(&other, true), Problem::BadSettingsFile);
    }

    #[test]
    fn each_kind_of_error_has_its_own_title_and_asks_for_its_own_action() {
        let expected = [
            (
                ErrorKind::Communication,
                Title::Communication,
                Hint::CheckCable,
            ),
            (ErrorKind::Audio, Title::Audio, Hint::CloseAudioUsers),
            (ErrorKind::Refused, Title::Refused, Hint::ReplugDongle),
            (ErrorKind::Verify, Title::Verify, Hint::ReapplyOrReplug),
            (ErrorKind::Drift, Title::Drift, Hint::CloseIt),
            (ErrorKind::Unresponsive, Title::Unresponsive, Hint::ReplugIt),
            (ErrorKind::Fatal, Title::Fatal, Hint::ReapplyOrRestart),
            (ErrorKind::Config, Title::Other, Hint::ReapplyRetry),
            (ErrorKind::Invalid, Title::Other, Hint::ReapplyRetry),
        ];
        for (kind, title, hint) in expected {
            let mut state = snapshot(Phase::Connected, Some(held(Mode::Quiet)));
            state.error = Some(failure(kind));
            let status = of(&state);
            assert_eq!((status.title, status.hint), (title, hint), "{kind}");
            assert_eq!(status.tone, Tone::Critical, "{kind}");
            assert!(status.details, "{kind}");
        }
    }

    #[test]
    fn details_exist_only_where_the_engine_has_an_error_to_log() {
        let mut state = snapshot(Phase::Connected, Some(held(Mode::Balanced)));
        assert!(!of(&state).details);
        assert!(!status(None, None, false).details);

        let mut other = held(Mode::Balanced);
        other.firmware_supported = false;
        state.device = Some(other);
        let unsupported = of(&state);
        assert_eq!(unsupported.title, Title::UnsupportedFirmware);
        assert!(!unsupported.details);

        state.device = Some(held(Mode::Balanced));
        state.save_error = Some(failure(ErrorKind::Config));
        assert!(of(&state).details);
    }

    #[test]
    fn a_stopped_engine_names_its_error_and_can_be_restarted() {
        let mut state = snapshot(Phase::Stopped, None);
        state.error = Some(failure(ErrorKind::Fatal));
        let status = of(&state);
        assert_eq!(status.title, Title::Fatal);
        assert!(status.reapply);
        assert!(!status.controls);
    }

    #[test]
    fn an_unsupported_firmware_is_named_whatever_the_engine_made_of_it() {
        let mut other = held(Mode::Balanced);
        other.firmware_supported = false;
        let mut state = snapshot(Phase::Connected, Some(other));
        state.error = Some(failure(ErrorKind::Refused));
        let status = of(&state);
        assert_eq!(
            (status.title, status.hint),
            (Title::UnsupportedFirmware, Hint::LeaveAlone)
        );

        state.error = None;
        assert_eq!(of(&state).title, Title::UnsupportedFirmware);

        // a dongle that stops answering matters more
        state.error = Some(failure(ErrorKind::Unresponsive));
        assert_eq!(of(&state).title, Title::Unresponsive);
    }

    #[test]
    fn the_settings_file_is_saved_pending_or_failing() {
        let mut state = snapshot(Phase::Connected, Some(held(Mode::Balanced)));
        assert_eq!(of(&state).hint, Hint::Saved);

        state.settings_saved = false;
        assert_eq!(of(&state).hint, Hint::Saving);

        state.save_error = Some(failure(ErrorKind::Config));
        let failing = of(&state);
        // a file that cannot be written is not a problem with the dongle: the dongle's state
        // stays in the title of a success, the failure takes the line
        assert_eq!(
            (failing.title, failing.hint, failing.tone),
            (Title::SaveFailed, Hint::CheckFile, Tone::Caution)
        );
    }

    #[test]
    fn only_the_most_important_problem_is_shown() {
        let mut state = snapshot(Phase::Connected, Some(held(Mode::Balanced)));
        state.save_error = Some(failure(ErrorKind::Config));
        state.error = Some(failure(ErrorKind::Communication));
        let shown = status(Some(&state), None, true);
        assert_eq!(
            (shown.title, shown.hint, shown.tone),
            (Title::Communication, Hint::CheckCable, Tone::Critical)
        );
    }

    #[test]
    fn something_unusual_points_to_the_log_and_gives_way_to_a_problem_or_a_failed_save() {
        let state = snapshot(Phase::Connected, Some(held(Mode::Balanced)));
        let warned = status(Some(&state), None, true);
        assert_eq!(
            (warned.title, warned.hint, warned.tone),
            (Title::InSync, Hint::SeeLog, Tone::Caution)
        );

        let mut failing = state;
        failing.save_error = Some(failure(ErrorKind::Config));
        assert_eq!(status(Some(&failing), None, true).hint, Hint::CheckFile);
    }

    #[test]
    fn re_apply_has_nothing_to_do_without_a_dongle() {
        assert!(!of(&snapshot(Phase::Disconnected, None)).reapply);
        for phase in [Phase::Waiting, Phase::Applying, Phase::Connected] {
            assert!(of(&snapshot(phase, None)).reapply, "{phase:?}");
        }
    }
}
