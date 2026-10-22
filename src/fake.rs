//! In-memory stand-in for the dongle: it speaks the HID report protocol against a 64 KiB CPU
//! memory image and models just enough firmware behaviour (playback state, analog references,
//! which routine the hook runs) to check the safety rules for changing the hook. Its images are
//! written from the documented firmware bytes, independently of the patch code, so a typo there
//! makes tests fail.
//!
//! A stream is seen by the codec only after a few reads, as on the real device, where the
//! endpoint takes a moment to start the USB stream after Windows says it runs.
//!
//! The microphone side models the DSP settings: a capture stream makes the ROM evaluate the
//! noise-reduction and AGC bits of `$1151` (as the stream-open reload does), a NEWC request in
//! `$1154` does the same when it is consumed, and sidetone follows its bit within a pass while
//! a capture and a playback stream are both open.
//!
//! Every write is checked as it arrives, and a violation panics, so a planner or guard bug fails
//! the test that triggers it:
//! - only the allowed regions are written;
//! - the hook, its operands and the routine are written only while the codec sees a silent
//!   stream and the device is fully active, and the routine and operands only while the hook
//!   is RTS;
//! - the hook never leaves a live state to RTS while the analog references are off;
//! - a live hook always runs a whole known routine;
//! - `$1151` holds only the noise-reduction, AGC and sidetone bits, `$1154` is only ever written
//!   with its NEWC bit added to the byte it holds, the sidetone level stays in its range, and
//!   none of the three is written while the chip-mode or DSP-bypass flags are set.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::mode::{Mode, Stream};
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
const DSP_ENABLE: usize = 0x1151;
const NEWC: usize = 0x1154;
const SIDETONE_LEVEL: usize = 0x1276;
const MIC_STREAM: usize = 0x13DC;
const PLAYBACK_STREAM: usize = 0x13DD;
const APPLIERS: usize = 0x00A4;
const MIXER: usize = 0x00A8;

/// Reads between a stream opening and the codec showing it.
const STREAM_DELAY_READS: usize = 10;

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
    eeprom: Vec<u8>,
    eeprom_reads: usize,
    writes: Vec<(u16, Vec<u8>)>,
    ignore_writes: bool,
    fail_after_writes: Option<usize>,
    no_endpoint: bool,
    /// The stream is running on the host side.
    stream_open: bool,
    /// The codec shows the stream: it follows `stream_open` after `stream_delay` reads.
    stream_seen: bool,
    stream_delay: usize,
    reads_until_seen: usize,
    /// The stream stops after this many writes.
    stop_stream_after_writes: Option<usize>,
    /// Writes that arrived while a stream was open but not yet shown by the codec.
    early_writes: usize,
    /// Every report the device has been sent, reads and writes.
    requests: usize,
    /// The device restarts when it is sent this many reports.
    restart_at_request: Option<usize>,
    /// The dongle is unplugged: every report fails.
    unplugged: bool,
    streams_opened: usize,
    pending: Option<Vec<u8>>,
    /// A capture stream is open.
    mic_open: bool,
    /// Reads until the ROM consumes a NEWC request; `None` when none is pending.
    newc_reads: Option<usize>,
    /// Reads the ROM takes to consume a NEWC request.
    newc_delay: usize,
    /// The ROM never consumes NEWC requests.
    newc_stuck: bool,
    /// Bits of `$1154` the ROM flips when it consumes a request, as a stream event would.
    newc_flip: u8,
    /// Bytes the ROM sets when it consumes a request, without updating `$1154`.
    newc_poke: Vec<(usize, u8)>,
    newc_consumed: usize,
}

/// A handle to the fake dongle; clones share the same device.
#[derive(Clone)]
pub struct FakeDevice(Arc<Mutex<Inner>>);

/// An open silent stream. Dropping it closes the stream.
pub struct FakeStream(FakeDevice);

/// An open microphone capture stream. Dropping it closes the stream.
pub struct FakeMic(FakeDevice);

impl Drop for FakeMic {
    fn drop(&mut self) {
        let mut d = self.0.lock();
        d.mic_open = false;
        d.mem[NEWC] &= !0x02;
        d.evaluate();
        if !std::thread::panicking() {
            d.settle();
        }
    }
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        let mut d = self.0.lock();
        d.stop_stream();
        if !std::thread::panicking() {
            d.settle();
        }
    }
}

impl Stream for FakeStream {
    fn check(&self) -> Result<()> {
        if self.0.lock().stream_open {
            Ok(())
        } else {
            Err(Error::Audio("the silent stream stopped".into()))
        }
    }
}

impl Inner {
    fn stop_stream(&mut self) {
        self.stream_open = false;
        self.stream_seen = false;
    }

    /// The dongle restarts: `fresh` is the memory the vendor patch loads, and every stream is gone.
    fn restart(&mut self, fresh: Vec<u8>) {
        self.mem = fresh;
        self.stream_open = false;
        self.stream_seen = false;
        self.mic_open = false;
        self.pending = None;
        self.newc_reads = None;
        self.settle();
    }

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
        self.mem[MIC_STREAM] = u8::from(self.mic_open);
        self.mem[PLAYBACK_STREAM] = u8::from(self.stream_seen);
        self.mem[NEWC] = (self.mem[NEWC] & !0x12)
            | (u8::from(self.mic_open) << 1)
            | (u8::from(self.stream_seen) << 4);
        let sidetone = self.mic_open && self.stream_seen && self.mem[DSP_ENABLE] & 0x80 != 0;
        self.mem[MIXER] = (u8::from(self.mic_open) << 5)
            | (u8::from(self.stream_seen) << 4)
            | (u8::from(sidetone) << 3);
        self.settle_playback();
    }

    /// The ROM evaluates the noise-reduction and AGC bits while a capture stream is open: at
    /// the stream's own reload and when it consumes a NEWC request.
    fn evaluate(&mut self) {
        let bits = self.mem[DSP_ENABLE];
        self.mem[APPLIERS] = if self.mic_open {
            ((bits & 0x04) << 5) | ((bits & 0x10) << 1)
        } else {
            0
        };
    }

    fn consume_newc(&mut self) {
        let Some(reads) = self.newc_reads else {
            return;
        };
        if self.newc_stuck {
            return;
        }
        if reads > 1 {
            self.newc_reads = Some(reads - 1);
            return;
        }
        self.newc_reads = None;
        self.mem[NEWC] = (self.mem[NEWC] & !0x01) ^ self.newc_flip;
        for (addr, value) in self.newc_poke.drain(..) {
            self.mem[addr] = value;
        }
        self.newc_consumed += 1;
        self.evaluate();
    }

    fn settle_playback(&mut self) {
        let running = self.running();
        if self.stream_seen {
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
        self.stream_seen
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
                || inside(MIC, MIC)
                || inside(DSP_ENABLE, DSP_ENABLE)
                || inside(NEWC, NEWC)
                || inside(SIDETONE_LEVEL, SIDETONE_LEVEL + 1),
            "write of {data:02x?} to ${addr:04X} is outside the allowed regions"
        );
        if inside(DSP_ENABLE, DSP_ENABLE)
            || inside(NEWC, NEWC)
            || inside(SIDETONE_LEVEL, SIDETONE_LEVEL + 1)
        {
            assert_eq!(
                self.mem[0x1000] & 0x80,
                0,
                "DSP write with the chip-mode flag set"
            );
            assert_eq!(
                self.mem[0x00A6] & 0x01,
                0,
                "DSP write while the DSP is bypassed"
            );
            assert_eq!(self.mem[0x1005], 0x03, "DSP write on an unknown chip id");
        }
        if inside(DSP_ENABLE, DSP_ENABLE) {
            let allowed = 0x04 | 0x10 | 0x80;
            assert_eq!(self.mem[DSP_ENABLE] & !allowed, 0, "$1151 holds other bits");
            assert_eq!(
                data[0] & !allowed,
                0,
                "write of {data:02x?} to $1151 sets other bits"
            );
        }
        if inside(NEWC, NEWC) {
            let current = self.mem[NEWC];
            assert_eq!(current & 0x01, 0, "NEWC requested while one is pending");
            assert_eq!(
                data[0],
                current | 0x01,
                "$1154 written with more than the NEWC bit"
            );
        }
        if inside(SIDETONE_LEVEL, SIDETONE_LEVEL + 1) {
            assert_eq!(data.len(), 2, "partial sidetone level write");
            let level = u16::from_le_bytes([data[0], data[1]]);
            assert!((1..=0x1000).contains(&level), "sidetone level {level:#06x}");
        }
        let touches_hook = addr <= HOOK + 2 && end > HOOK;
        let touches_window = addr <= WINDOW_END && end > WINDOW;
        let settle_only = addr == SETTLE && data.len() == 1;
        if (touches_hook || touches_window) && !settle_only {
            assert!(
                self.active(),
                "write to ${addr:04X} while the codec does not show an active silent stream"
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
    /// The device state. A poisoned lock is used as it is: the panic that poisoned it is the
    /// test failure, and the fake's own drops must not add a second one.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

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
        put(0x1EF9, &[0x68, 0x68, 0x60]);
        put(0x1001, &[0x15, 0x00, 0x00, 0x00, 0x03]);
        put(MIC, &[0x3C]);
        put(REFS, &[0x02]);
        put(0x1000, &[0x01]);
        put(DSP_ENABLE, &[0x04]);
        put(SIDETONE_LEVEL, &[0xB3, 0x03]);
        let dev = Self(Arc::new(Mutex::new(Inner {
            mem,
            eeprom: vec![0; 0x4000],
            eeprom_reads: 0,
            writes: Vec::new(),
            ignore_writes: false,
            fail_after_writes: None,
            no_endpoint: false,
            stream_open: false,
            stream_seen: false,
            stream_delay: STREAM_DELAY_READS,
            reads_until_seen: 0,
            stop_stream_after_writes: None,
            early_writes: 0,
            requests: 0,
            restart_at_request: None,
            unplugged: false,
            streams_opened: 0,
            pending: None,
            mic_open: false,
            newc_reads: None,
            newc_delay: 1,
            newc_stuck: false,
            newc_flip: 0,
            newc_poke: Vec::new(),
            newc_consumed: 0,
        })));
        dev.lock().settle();
        dev
    }

    /// Opens a capture stream: the ROM reloads the DSP settings for it.
    pub fn open_mic(&self) -> FakeMic {
        let mut d = self.lock();
        d.mic_open = true;
        d.mem[NEWC] |= 0x02;
        d.evaluate();
        d.settle();
        FakeMic(self.clone())
    }

    /// Flag bits of the noise-reduction, AGC and sidetone appliers as the ROM last evaluated them.
    pub fn appliers(&self) -> u8 {
        self.byte(APPLIERS as u16)
    }

    /// Bit 3 of the mixer flags: the sidetone path is running.
    pub fn sidetone_running(&self) -> bool {
        self.byte(MIXER as u16) & 0x08 != 0
    }

    /// NEWC requests the ROM has consumed.
    pub fn newc_consumed(&self) -> usize {
        self.lock().newc_consumed
    }

    /// The ROM takes `reads` reads to consume a NEWC request (default 1).
    pub fn consume_newc_after_reads(&self, reads: usize) {
        self.lock().newc_delay = reads;
    }

    /// The ROM never consumes a NEWC request.
    pub fn never_consume_newc(&self) {
        self.lock().newc_stuck = true;
    }

    /// The ROM raises a NEWC request of its own, as it does on a stream event, and consumes it
    /// at the `reads`th read request from now.
    pub fn raise_newc(&self, reads: usize) {
        let mut d = self.lock();
        d.mem[NEWC] |= 0x01;
        d.newc_reads = Some(reads);
    }

    /// The ROM flips these bits of `$1154` when it consumes a request, as a stream event
    /// arriving at that moment would.
    pub fn flip_on_newc(&self, bits: u8) {
        self.lock().newc_flip = bits;
    }

    /// When the ROM consumes a request it also sets the byte at `addr`, as a stream event
    /// arriving at that moment would, while `$1154` keeps the stream bits it had.
    pub fn poke_on_newc(&self, addr: u16, value: u8) {
        self.lock().newc_poke.push((addr as usize, value));
    }

    /// Replaces the EEPROM image, which is 16 KiB.
    pub fn set_eeprom(&self, image: &[u8]) {
        let mut d = self.lock();
        assert_eq!(image.len(), d.eeprom.len());
        d.eeprom.copy_from_slice(image);
    }

    /// EEPROM read requests the device has been sent.
    pub fn eeprom_reads(&self) -> usize {
        self.lock().eeprom_reads
    }

    pub fn mem(&self) -> Vec<u8> {
        self.lock().mem.clone()
    }

    pub fn byte(&self, addr: u16) -> u8 {
        self.lock().mem[addr as usize]
    }

    /// Overwrites memory directly, as test setup; the device is left idle with no stream.
    pub fn poke(&self, addr: u16, data: &[u8]) {
        let mut d = self.lock();
        let a = addr as usize;
        d.mem[a..a + data.len()].copy_from_slice(data);
    }

    /// Brings the playback registers in line with memory that was just poked.
    pub fn settle(&self) {
        self.lock().settle();
    }

    pub fn writes(&self) -> Vec<(u16, Vec<u8>)> {
        self.lock().writes.clone()
    }

    pub fn clear_writes(&self) {
        self.lock().writes.clear();
    }

    /// Write requests are recorded but do not change memory.
    pub fn ignore_writes(&self) {
        self.lock().ignore_writes = true;
    }

    /// The device stops accepting writes after `n` more have gone through, as if unplugged.
    pub fn fail_after_writes(&self, n: usize) {
        let mut d = self.lock();
        d.fail_after_writes = Some(d.writes.len() + n);
    }

    pub fn clear_failure(&self) {
        self.lock().fail_after_writes = None;
    }

    /// Every report sent to the device so far, reads and writes.
    pub fn requests(&self) -> usize {
        self.lock().requests
    }

    /// Every report fails from now on, as if the cable were pulled.
    pub fn unplug(&self) {
        self.lock().unplugged = true;
    }

    /// The device answers again. Its memory is as it was: use [`FakeDevice::replug`] for a
    /// device that restarted.
    pub fn plug_in(&self) {
        self.lock().unplugged = false;
    }

    /// The dongle restarted: the vendor patch is loaded afresh and every stream is gone.
    pub fn replug(&self) {
        let fresh = FakeDevice::stock().mem();
        self.lock().restart(fresh);
    }

    /// The dongle restarts when it is sent its `n`th report from now.
    pub fn replug_at_request(&self, n: usize) {
        let mut d = self.lock();
        d.restart_at_request = Some(d.requests + n);
    }

    /// Opening a stream fails, as if the endpoint were missing.
    pub fn remove_endpoint(&self) {
        self.lock().no_endpoint = true;
    }

    pub fn streams_opened(&self) -> usize {
        self.lock().streams_opened
    }

    pub fn stream_is_open(&self) -> bool {
        self.lock().stream_open
    }

    /// Writes made after a stream was opened but before the codec showed it.
    pub fn early_writes(&self) -> usize {
        self.lock().early_writes
    }

    /// Opens a stream that the codec already shows.
    pub fn open_active_stream(&self) -> Result<FakeStream> {
        let stream = self.open_stream()?;
        let mut d = self.lock();
        d.stream_seen = true;
        d.settle();
        Ok(stream)
    }

    /// True once the codec shows the stream.
    pub fn codec_sees_stream(&self) -> bool {
        self.lock().stream_seen
    }

    /// The codec never shows a stream that is opened, as if the endpoint were not the one
    /// behind the dongle's output.
    pub fn never_show_streams(&self) {
        self.lock().stream_delay = usize::MAX;
    }

    /// The running stream stops after `n` more writes, as if the audio thread had failed.
    pub fn stop_stream_after_writes(&self, n: usize) {
        let mut d = self.lock();
        d.stop_stream_after_writes = Some(d.writes.len() + n);
    }

    pub fn open_stream(&self) -> Result<FakeStream> {
        let mut d = self.lock();
        if d.no_endpoint {
            return Err(Error::Audio("no playback endpoint".into()));
        }
        d.stream_open = true;
        d.stream_seen = false;
        d.reads_until_seen = d.stream_delay;
        d.streams_opened += 1;
        d.settle();
        Ok(FakeStream(self.clone()))
    }
}

impl Transport for FakeDevice {
    fn send(&mut self, report: &OutReport) -> Result<()> {
        let mut d = self.lock();
        if d.unplugged {
            return Err(Error::Device("device disconnected".into()));
        }
        d.requests += 1;
        if d.restart_at_request == Some(d.requests) {
            d.restart_at_request = None;
            let fresh = FakeDevice::stock().mem();
            d.restart(fresh);
        }
        let b = report.as_bytes();
        assert_eq!(b[0], 0x04, "OUT report id");
        let len = b[2] as usize;
        let addr = u16::from_be_bytes([b[3], b[4]]) as usize;
        assert!((1..=32).contains(&len));
        match b[1] {
            0x00 => {
                assert!(b[5..].iter().all(|&x| x == 0), "data in read request");
                d.consume_newc();
                d.pending = Some(d.mem[addr..addr + len].to_vec());
            }
            0x40 => {
                if d.fail_after_writes.is_some_and(|n| d.writes.len() >= n) {
                    return Err(Error::Device("device disconnected".into()));
                }
                let data = b[5..5 + len].to_vec();
                d.check_write(addr, &data);
                if d.stream_open && !d.stream_seen {
                    d.early_writes += 1;
                }
                if !d.ignore_writes {
                    d.mem[addr..addr + len].copy_from_slice(&data);
                    if addr == NEWC && data[0] & 0x01 != 0 {
                        d.newc_reads = Some(d.newc_delay);
                    }
                    d.settle();
                }
                d.writes.push((addr as u16, data));
                if d.stop_stream_after_writes == Some(d.writes.len()) {
                    d.stop_stream();
                    d.settle();
                }
            }
            0x20 => {
                assert!(
                    b[5..].iter().all(|&x| x == 0),
                    "data in EEPROM read request"
                );
                assert!(addr + len <= d.eeprom.len(), "EEPROM read out of range");
                d.eeprom_reads += 1;
                d.pending = Some(d.eeprom[addr..addr + len].to_vec());
            }
            flags => panic!("forbidden flags {flags:#04x}"),
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Vec<u8>> {
        let mut r = vec![0u8; 33];
        let mut d = self.lock();
        if d.unplugged {
            return Err(Error::Device("device disconnected".into()));
        }
        if let Some(data) = d.pending.take() {
            r[0] = 0x05;
            r[1..1 + data.len()].copy_from_slice(&data);
            if d.stream_open && !d.stream_seen {
                d.reads_until_seen = d.reads_until_seen.saturating_sub(1);
                if d.reads_until_seen == 0 {
                    d.stream_seen = true;
                    d.settle();
                }
            }
        }
        Ok(r)
    }
}
