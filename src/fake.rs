//! In-memory stand-in for the dongle: it speaks the HID report protocol against a 64 KiB CPU
//! memory image and models just enough firmware behaviour (playback state, analog references,
//! which routine the hook runs) to check the safety rules for changing the hook. Its images are
//! written from the documented firmware bytes, independently of the patch code, so a typo there
//! makes tests fail.
//!
//! Every write is checked as it arrives, and a violation panics, so a planner or guard bug fails
//! the test that triggers it:
//! - only the allowed regions are written;
//! - the hook, its operands and the routine are written only while a silent stream has the
//!   device fully active, and the routine and operands only while the hook is RTS;
//! - the hook never leaves a live state to RTS while the analog references are off;
//! - a live hook always runs a whole known routine.

use std::cell::RefCell;
use std::rc::Rc;

use crate::mode::Mode;
use crate::protocol::{OutReport, Transport};
use crate::{Error, Result};

pub fn bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

pub const VENDOR_HEX: &str =
    "2fbf1bcd8512f0168d85128dea13a9018d84128de913a9028de813201fab606b00bad4bfd4a9008d1a0f60";
pub const BALANCED_HEX: &str =
    "f2180f011a8fa517e23e0f0209c2023e0fb250d7579e5f9ee6d2771a0f80def23e0f02032071d61f9dd360";
pub const MIN_HISS_HEX: &str =
    "e2180f011ce21a0f01f5b20ac9179de21a0f0409c2441a0fb23cd7579e5f9edd60d2771a0fd2331b0f80d2";

const HOOK: usize = 0x14B1;
const WINDOW: usize = 0x1EFC;
const WINDOW_END: usize = 0x1F26;
const SETTLE: usize = 0x1F15;
const DAC_A: usize = 0x16CF;
const DAC_B: usize = 0x17D0;
const MIC: usize = 0x1017;
const ENABLE: usize = 0x0F18;
const DRIVER: usize = 0x0F1A;
const DRIVER2: usize = 0x0F1B;
const REFS: usize = 0x0F3E;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Running {
    /// The vendor routine: driver forced on.
    Vendor,
    Balanced,
    MinHiss,
    /// The hook returns at once and the ROM drives the output.
    Rom,
}

struct Inner {
    mem: Vec<u8>,
    writes: Vec<(u16, Vec<u8>)>,
    ignore_writes: bool,
    fail_after_writes: Option<usize>,
    no_endpoint: bool,
    stream_open: bool,
    streams_opened: usize,
    pending: Option<Vec<u8>>,
}

/// A handle to the fake dongle; clones share the same device.
#[derive(Clone)]
pub struct FakeDevice(Rc<RefCell<Inner>>);

/// An open silent stream. Dropping it closes the stream.
pub struct FakeStream(FakeDevice);

impl Drop for FakeStream {
    fn drop(&mut self) {
        let mut d = self.0.0.borrow_mut();
        d.stream_open = false;
        if !std::thread::panicking() {
            d.settle();
        }
    }
}

impl Inner {
    fn running(&self) -> Running {
        let [op, lo, hi, next] = self.mem[HOOK..HOOK + 4] else {
            unreachable!()
        };
        assert_eq!(next, 0x4C, "the next hook entry was changed");
        if op == 0x60 {
            return Running::Rom;
        }
        assert_eq!(op, 0x4C, "hook opcode is neither JMP nor RTS");
        let window = &self.mem[WINDOW..=WINDOW_END];
        match [lo, hi] {
            [0x1B, 0x1F] => {
                assert_eq!(
                    window,
                    bytes(VENDOR_HEX),
                    "live vendor hook, damaged routine"
                );
                Running::Vendor
            }
            [0xFC, 0x1E] if window == bytes(BALANCED_HEX) => Running::Balanced,
            [0xFC, 0x1E] if min_hiss_with_any_settle(window) => Running::MinHiss,
            _ => panic!(
                "live hook leads to an unknown routine: {:02x?}",
                &self.mem[HOOK..HOOK + 4]
            ),
        }
    }

    /// Brings the playback registers to what the firmware would show for the current routine,
    /// hook and stream state.
    fn settle(&mut self) {
        let running = self.running();
        if self.stream_open {
            self.mem[ENABLE] |= 0x03;
            self.mem[DRIVER] = 0x00;
            self.mem[DRIVER2] = 0x00;
            if running == Running::Balanced {
                self.mem[REFS] |= 0x02;
            }
            return;
        }
        let held = (self.mem[DAC_A] | self.mem[DAC_B]) & 0x03;
        self.mem[ENABLE] = held;
        self.mem[DRIVER2] = 0x33;
        match running {
            Running::Vendor => self.mem[DRIVER] = 0x00,
            Running::Balanced => {
                self.mem[DRIVER] = 0x77;
                self.mem[REFS] &= !0x02;
            }
            Running::MinHiss => self.mem[DRIVER] = 0x77,
            Running::Rom => self.mem[DRIVER] = if held == 0 { 0x33 } else { 0x00 },
        }
    }

    fn active(&self) -> bool {
        self.stream_open
            && self.mem[ENABLE] & 1 != 0
            && self.mem[DRIVER] == 0x00
            && self.mem[REFS] & 0x02 != 0
    }

    fn check_write(&self, addr: usize, data: &[u8]) {
        let end = addr + data.len();
        let inside = |lo: usize, hi: usize| addr >= lo && end <= hi + 1;
        assert!(
            inside(HOOK, HOOK + 2)
                || inside(DAC_A, DAC_A)
                || inside(DAC_B, DAC_B)
                || inside(WINDOW, WINDOW_END)
                || inside(MIC, MIC),
            "write of {data:02x?} to ${addr:04X} is outside the allowed regions"
        );
        let touches_hook = addr <= HOOK + 2 && end > HOOK;
        let touches_window = addr <= WINDOW_END && end > WINDOW;
        let settle_only = addr == SETTLE && data.len() == 1;
        if (touches_hook || touches_window) && !settle_only {
            assert!(
                self.active(),
                "write to ${addr:04X} without an active silent stream"
            );
        }
        if ((addr > HOOK && touches_hook) || touches_window) && !settle_only {
            assert_eq!(
                self.mem[HOOK], 0x60,
                "write to ${addr:04X} while the hook is live"
            );
        }
        if addr == HOOK && data == [0x60] && self.mem[HOOK] == 0x4C {
            assert!(
                self.mem[REFS] & 0x02 != 0,
                "hook turned off while the analog references are off"
            );
        }
    }
}

fn min_hiss_with_any_settle(window: &[u8]) -> bool {
    let want = bytes(MIN_HISS_HEX);
    let i = SETTLE - WINDOW;
    window[..i] == want[..i] && window[i + 1..] == want[i + 1..] && (2..=0xC8).contains(&window[i])
}

impl FakeDevice {
    /// Memory as the vendor patch leaves it after plug-in, with the device idle.
    pub fn stock() -> Self {
        Self::in_mode(Mode::Stock)
    }

    /// The device idle in `mode`, with the settle time at its default.
    pub fn in_mode(mode: Mode) -> Self {
        let (hold, hook, window) = match mode {
            Mode::Stock => (0x03, "4c1b1f4c", VENDOR_HEX),
            Mode::Quiet => (0x00, "601b1f4c", VENDOR_HEX),
            Mode::Balanced => (0x00, "4cfc1e4c", BALANCED_HEX),
            Mode::MinHiss => (0x00, "4cfc1e4c", MIN_HISS_HEX),
        };
        let mut mem = vec![0u8; 0x1_0000];
        let mut put = |addr: usize, b: &[u8]| mem[addr..addr + b.len()].copy_from_slice(b);
        put(HOOK, &bytes(hook));
        put(0x16C9, &bytes("ad180f29df09008d180f"));
        put(0x17CA, &bytes("ad180f29df09008d180f"));
        put(DAC_A, &[hold]);
        put(DAC_B, &[hold]);
        put(WINDOW, &bytes(window));
        put(MIC, &[0x3C]);
        put(REFS, &[0x02]);
        let dev = Self(Rc::new(RefCell::new(Inner {
            mem,
            writes: Vec::new(),
            ignore_writes: false,
            fail_after_writes: None,
            no_endpoint: false,
            stream_open: false,
            streams_opened: 0,
            pending: None,
        })));
        dev.0.borrow_mut().settle();
        dev
    }

    pub fn mem(&self) -> Vec<u8> {
        self.0.borrow().mem.clone()
    }

    pub fn byte(&self, addr: u16) -> u8 {
        self.0.borrow().mem[addr as usize]
    }

    /// Overwrites memory directly, as test setup; the device is left idle with no stream.
    pub fn poke(&self, addr: u16, data: &[u8]) {
        let mut d = self.0.borrow_mut();
        let a = addr as usize;
        d.mem[a..a + data.len()].copy_from_slice(data);
    }

    /// Brings the playback registers in line with memory that was just poked.
    pub fn settle(&self) {
        self.0.borrow_mut().settle();
    }

    pub fn writes(&self) -> Vec<(u16, Vec<u8>)> {
        self.0.borrow().writes.clone()
    }

    pub fn clear_writes(&self) {
        self.0.borrow_mut().writes.clear();
    }

    /// Write requests are recorded but do not change memory.
    pub fn ignore_writes(&self) {
        self.0.borrow_mut().ignore_writes = true;
    }

    /// The device stops accepting writes after `n` more have gone through, as if unplugged.
    pub fn fail_after_writes(&self, n: usize) {
        let mut d = self.0.borrow_mut();
        d.fail_after_writes = Some(d.writes.len() + n);
    }

    pub fn clear_failure(&self) {
        self.0.borrow_mut().fail_after_writes = None;
    }

    /// Opening a stream fails, as if the endpoint were missing.
    pub fn remove_endpoint(&self) {
        self.0.borrow_mut().no_endpoint = true;
    }

    pub fn streams_opened(&self) -> usize {
        self.0.borrow().streams_opened
    }

    pub fn stream_is_open(&self) -> bool {
        self.0.borrow().stream_open
    }

    pub fn open_stream(&self) -> Result<FakeStream> {
        let mut d = self.0.borrow_mut();
        if d.no_endpoint {
            return Err(Error::Device("no playback endpoint".into()));
        }
        d.stream_open = true;
        d.streams_opened += 1;
        d.settle();
        Ok(FakeStream(self.clone()))
    }
}

impl Transport for FakeDevice {
    fn send(&mut self, report: &OutReport) -> Result<()> {
        let mut d = self.0.borrow_mut();
        let b = report.as_bytes();
        assert_eq!(b[0], 0x04, "OUT report id");
        let len = b[2] as usize;
        let addr = u16::from_be_bytes([b[3], b[4]]) as usize;
        assert!((1..=32).contains(&len));
        match b[1] {
            0x00 => {
                assert!(b[5..].iter().all(|&x| x == 0), "data in read request");
                d.pending = Some(d.mem[addr..addr + len].to_vec());
            }
            0x40 => {
                if d.fail_after_writes.is_some_and(|n| d.writes.len() >= n) {
                    return Err(Error::Device("device disconnected".into()));
                }
                let data = b[5..5 + len].to_vec();
                d.check_write(addr, &data);
                if !d.ignore_writes {
                    d.mem[addr..addr + len].copy_from_slice(&data);
                    d.settle();
                }
                d.writes.push((addr as u16, data));
            }
            flags => panic!("forbidden flags {flags:#04x}"),
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Vec<u8>> {
        let mut r = vec![0u8; 33];
        if let Some(data) = self.0.borrow_mut().pending.take() {
            r[0] = 0x05;
            r[1..1 + data.len()].copy_from_slice(&data);
        }
        Ok(r)
    }
}
