//! Notices a call into the device that never returns.
//!
//! hidapi waits for the answer to a request without a time limit, so a dongle or driver that
//! stops answering blocks the worker thread for good. Every call into the device is marked here
//! while it runs; the engine's watchdog thread asks whether one has run too long.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::Result;
use crate::protocol::{OutReport, Transport};

#[derive(Clone, Default)]
pub(super) struct CallWatch(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    call: Option<Call>,
    /// How long the last call that was reported as stuck took in the end.
    recovered: Option<Duration>,
}

struct Call {
    what: &'static str,
    since: Instant,
    reported: bool,
}

impl CallWatch {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Marks the start of a call; dropping the guard marks its end.
    pub(super) fn enter(&self, what: &'static str) -> CallGuard {
        self.state().call = Some(Call {
            what,
            since: Instant::now(),
            reported: false,
        });
        CallGuard(self.clone())
    }

    /// The call that has been running for at least `limit` and has not been reported yet: what
    /// it is and how long it has run. Each call is reported once.
    pub(super) fn stuck(&self, limit: Duration) -> Option<(&'static str, Duration)> {
        let mut state = self.state();
        let call = state.call.as_mut()?;
        let running = call.since.elapsed();
        if call.reported || running < limit {
            return None;
        }
        call.reported = true;
        Some((call.what, running))
    }

    /// Whether a call that was reported as stuck is still running.
    pub(super) fn is_stuck(&self) -> bool {
        self.state().call.as_ref().is_some_and(|call| call.reported)
    }

    /// How long a call that was reported as stuck took in the end. Given once.
    pub(super) fn take_recovered(&self) -> Option<Duration> {
        self.state().recovered.take()
    }
}

pub(super) struct CallGuard(CallWatch);

impl Drop for CallGuard {
    fn drop(&mut self) {
        let mut state = self.0.state();
        if let Some(call) = state.call.take()
            && call.reported
        {
            state.recovered = Some(call.since.elapsed());
        }
    }
}

/// A transport whose every request is marked in a [`CallWatch`].
pub(super) struct Watched<D> {
    inner: D,
    watch: CallWatch,
}

impl<D> Watched<D> {
    pub(super) fn new(inner: D, watch: CallWatch) -> Watched<D> {
        Watched { inner, watch }
    }
}

impl<D: Transport> Transport for Watched<D> {
    fn send(&mut self, report: &OutReport) -> Result<()> {
        let _call = self.watch.enter("sending a request to the dongle");
        self.inner.send(report)
    }

    fn receive(&mut self) -> Result<Vec<u8>> {
        let _call = self.watch.enter("waiting for the dongle's answer");
        self.inner.receive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use crate::fake::FakeDevice;
    use crate::protocol::read_mem;

    const LIMIT: Duration = Duration::from_millis(50);

    /// A transport that is stuck once, in a request it is sent (`in_send`) or in the wait for an
    /// answer, until the test lets go.
    struct Stuck {
        in_send: bool,
        armed: bool,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Stuck {
        fn new(in_send: bool) -> (Stuck, mpsc::Receiver<()>, mpsc::Sender<()>) {
            let (entered, entered_rx) = mpsc::channel();
            let (release_tx, release) = mpsc::channel();
            let stuck = Stuck {
                in_send,
                armed: true,
                entered,
                release,
            };
            (stuck, entered_rx, release_tx)
        }

        fn block_if(&mut self, in_send: bool) {
            if self.armed && self.in_send == in_send {
                self.armed = false;
                self.entered.send(()).unwrap();
                let _ = self.release.recv_timeout(Duration::from_secs(30));
            }
        }
    }

    impl Transport for Stuck {
        fn send(&mut self, _: &OutReport) -> Result<()> {
            self.block_if(true);
            Ok(())
        }

        fn receive(&mut self) -> Result<Vec<u8>> {
            self.block_if(false);
            Ok(Vec::new())
        }
    }

    #[test]
    fn a_call_that_does_not_return_is_reported_once_and_its_return_is_noticed() {
        let watch = CallWatch::default();
        assert_eq!(watch.stuck(Duration::ZERO), None);

        let (stuck, entered_rx, release_tx) = Stuck::new(false);
        let mut dev = Watched::new(stuck, watch.clone());
        let worker = thread::spawn(move || {
            let _ = dev.receive();
        });
        entered_rx.recv().unwrap();

        // not yet: it has not run for the limit
        assert_eq!(watch.stuck(Duration::from_secs(3600)), None);
        assert!(!watch.is_stuck());
        thread::sleep(LIMIT);
        let (what, running) = watch.stuck(LIMIT).expect("the call is stuck");
        assert!(what.contains("answer"), "{what}");
        assert!(running >= LIMIT);
        assert!(watch.is_stuck());
        assert_eq!(watch.stuck(LIMIT), None, "reported once");
        assert_eq!(watch.take_recovered(), None);

        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(!watch.is_stuck());
        assert!(watch.take_recovered().unwrap() >= LIMIT);
        assert_eq!(watch.take_recovered(), None, "given once");
    }

    #[test]
    fn a_request_that_cannot_be_sent_is_reported_too() {
        let watch = CallWatch::default();
        let (stuck, entered_rx, release_tx) = Stuck::new(true);
        let mut dev = Watched::new(stuck, watch.clone());
        let worker = thread::spawn(move || {
            let _ = read_mem(&mut dev, 0x1001, 1);
        });
        entered_rx.recv().unwrap();
        thread::sleep(LIMIT);
        let (what, _) = watch.stuck(LIMIT).expect("the request is stuck");
        assert!(what.contains("sending"), "{what}");
        release_tx.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn calls_that_return_in_time_leave_nothing_behind() {
        let watch = CallWatch::default();
        let mut dev = Watched::new(
            FakeDevice::in_mode(crate::mode::Mode::Balanced),
            watch.clone(),
        );
        read_mem(&mut dev, 0x1001, 5).unwrap();
        assert_eq!(watch.stuck(Duration::ZERO), None);
        assert!(!watch.is_stuck());
        assert_eq!(watch.take_recovered(), None);
    }

    #[test]
    fn a_new_call_is_reported_again() {
        let watch = CallWatch::default();
        for _ in 0..2 {
            let call = watch.enter("a call");
            thread::sleep(Duration::from_millis(5));
            assert!(watch.stuck(Duration::ZERO).is_some());
            drop(call);
            assert!(watch.take_recovered().is_some());
        }
    }
}
