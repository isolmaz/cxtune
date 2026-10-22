//! The volume of the dongle's audio endpoints, on a thread of its own: the Windows audio service
//! answers when it likes. While the window is shown the thread reads the volume twice a second,
//! so that the sliders follow the buttons on the dongle and the Windows mixer.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use cxtune::volume::{Level, Side, Volume};

/// How often the volume is read while the window is shown.
const LOOK: Duration = Duration::from_millis(500);
/// The thread writes at most this often, so that a slider that is dragged does not flood the
/// dongle with requests.
const PACE: Duration = Duration::from_millis(40);
/// A missing endpoint is looked for again after this many reads.
const SEARCH_EVERY: u32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Levels {
    pub playback: Option<Level>,
    pub capture: Option<Level>,
}

impl Levels {
    pub fn of(&self, side: Side) -> Option<Level> {
        match side {
            Side::Playback => self.playback,
            Side::Capture => self.capture,
        }
    }
}

enum Message {
    Set(Side, u8),
    Mute(Side, bool),
    /// Whether the window is shown: the volume is read only then.
    Watch(bool),
}

/// What the thread has been asked to do since it last looked: the newest request of each kind.
#[derive(Default)]
struct Pending {
    percent: [Option<u8>; 2],
    muted: [Option<bool>; 2],
    watch: Option<bool>,
}

impl Pending {
    fn add(&mut self, message: Message) {
        match message {
            Message::Set(side, percent) => self.percent[side as usize] = Some(percent),
            Message::Mute(side, muted) => self.muted[side as usize] = Some(muted),
            Message::Watch(on) => self.watch = Some(on),
        }
    }

    fn writes(&self) -> bool {
        self.percent.iter().any(Option::is_some) || self.muted.iter().any(Option::is_some)
    }
}

pub struct Sound {
    requests: Sender<Message>,
}

impl Sound {
    /// Starts the thread. `report` runs on it with the levels that it has read.
    pub fn start(report: impl Fn(Levels) + Send + 'static) -> Sound {
        let (requests, inbox) = mpsc::channel();
        thread::Builder::new()
            .name("cxtune-volume".into())
            .spawn(move || run(&inbox, &report))
            .ok();
        Sound { requests }
    }

    pub fn set(&self, side: Side, percent: u8) {
        let _ = self.requests.send(Message::Set(side, percent));
    }

    pub fn mute(&self, side: Side, muted: bool) {
        let _ = self.requests.send(Message::Mute(side, muted));
    }

    pub fn watch(&self, on: bool) {
        let _ = self.requests.send(Message::Watch(on));
    }
}

fn run(inbox: &Receiver<Message>, report: &impl Fn(Levels)) {
    let Ok(mut volume) = Volume::open() else {
        return;
    };
    let mut watching = false;
    let mut reads = 0u32;
    loop {
        let first = if watching {
            match inbox.recv_timeout(LOOK) {
                Ok(message) => Some(message),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match inbox.recv() {
                Ok(message) => Some(message),
                Err(_) => return,
            }
        };
        let mut pending = Pending::default();
        let timed_out = first.is_none();
        for message in first.into_iter().chain(inbox.try_iter()) {
            pending.add(message);
        }

        for side in [Side::Playback, Side::Capture] {
            if let Some(percent) = pending.percent[side as usize] {
                let _ = volume.set_percent(side, percent);
            }
            if let Some(muted) = pending.muted[side as usize] {
                let _ = volume.set_muted(side, muted);
            }
        }
        if pending.writes() {
            thread::sleep(PACE);
        }
        if let Some(on) = pending.watch {
            watching = on;
        }

        if watching && (timed_out || pending.watch == Some(true)) {
            reads += 1;
            let mut levels = read(&mut volume);
            let missing = levels.playback.is_none() || levels.capture.is_none();
            if missing && reads % SEARCH_EVERY == 1 {
                volume.refresh();
                levels = read(&mut volume);
            }
            report(levels);
        }
    }
}

fn read(volume: &mut Volume) -> Levels {
    Levels {
        playback: volume.level(Side::Playback),
        capture: volume.level(Side::Capture),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_newest_request_of_each_kind_is_kept() {
        let mut pending = Pending::default();
        pending.add(Message::Set(Side::Playback, 10));
        pending.add(Message::Set(Side::Playback, 30));
        pending.add(Message::Set(Side::Capture, 70));
        pending.add(Message::Mute(Side::Playback, true));
        pending.add(Message::Mute(Side::Playback, false));
        pending.add(Message::Watch(true));
        pending.add(Message::Watch(false));
        assert_eq!(pending.percent, [Some(30), Some(70)]);
        assert_eq!(pending.muted, [Some(false), None]);
        assert_eq!(pending.watch, Some(false));
        assert!(pending.writes());
    }

    #[test]
    fn watching_alone_writes_nothing() {
        let mut pending = Pending::default();
        pending.add(Message::Watch(true));
        assert!(!pending.writes());
    }
}
