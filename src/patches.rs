//! Every write this app can make, and the single function that performs them.
//!
//! A [`Patch`] has private fields and is built only by the functions in this module, from the
//! firmware images and constants below. Each one names an address inside a fixed set of
//! writable patch-RAM regions, the bytes that must be there before the write, the new bytes,
//! and guard bytes or bits elsewhere in memory that must hold. All addresses are CPU-space
//! patch RAM, which the dongle reloads from EEPROM on every replug or USB reset.

use crate::protocol::{OutReport, Transport, read_mem};
use crate::{Error, Result, hex};

pub const HOOK: u16 = 0x14B1;
pub const HOOK_OPERANDS: u16 = 0x14B2;
pub const DAC_A_ROUTINE: u16 = 0x16C9;
pub const DAC_B_ROUTINE: u16 = 0x17CA;
pub const DAC_A: u16 = DAC_A_ROUTINE + 6;
pub const DAC_B: u16 = DAC_B_ROUTINE + 6;
pub const WINDOW: u16 = 0x1EFC;
pub const WINDOW_LEN: usize = 43;
pub const MIC_CONTROL: u16 = 0x1017;
/// DAC channel enables; bit 0 is set while a playback stream is running.
pub const DAC_ENABLE: u16 = 0x0F18;
/// Headphone driver power state; 0x00 is fully active.
pub const HP_DRIVER: u16 = 0x0F1A;
/// Second output driver: 0x00 while a stream plays, 0x33 at idle.
pub const HP_DRIVER2: u16 = 0x0F1B;
/// Analog power-up reference register; bit 1 is the bias reference.
pub const REFS: u16 = 0x0F3E;
pub const REFS_ON: u8 = 0x02;
/// Minimum-hiss settle time immediate (units of 10 ms).
pub const SETTLE_ADDR: u16 = 0x1F15;
pub const SETTLE_DEFAULT: u8 = 0x3C;
pub const SETTLE_MIN: u8 = 0x02;
pub const SETTLE_MAX: u8 = 0xC8;
/// The vendor hook-29 body, `PLA PLA RTS`: both of our routines leave through it.
const GADGET_ADDR: u16 = 0x1EF9;
const GADGET: [u8; 3] = [0x68, 0x68, 0x60];
/// Firmware version `15 00 00 00` and chip id `03` at $1001..$1005, the ROM the routines were
/// written against.
pub const ROM_ID_ADDR: u16 = 0x1001;
pub const ROM_ID: [u8; 5] = [0x15, 0x00, 0x00, 0x00, 0x03];

pub const JMP: u8 = 0x4C;
pub const RTS: u8 = 0x60;
pub const VENDOR_OPERANDS: [u8; 2] = [0x1B, 0x1F];
pub const WINDOW_OPERANDS: [u8; 2] = [0xFC, 0x1E];
/// `LDA $0F18; AND #$DF; ORA #` precedes, and `STA $0F18` follows, the DAC-hold immediate.
pub const DAC_PREFIX: [u8; 6] = [0xAD, 0x18, 0x0F, 0x29, 0xDF, 0x09];
pub const DAC_SUFFIX: [u8; 3] = [0x8D, 0x18, 0x0F];
pub const DAC_HOLD_ON: u8 = 0x03;
pub const DAC_HOLD_OFF: u8 = 0x00;

pub const MIC_MIN: u8 = 0x38;
pub const MIC_MAX: u8 = 0x3F;

const VENDOR_TAIL: [u8; 3] = [0x1B, 0x1F, 0x4C];
const WINDOW_TAIL: [u8; 3] = [0xFC, 0x1E, 0x4C];

/// The 43 bytes at $1EFC..$1F26 as the vendor patch leaves them (unused by the vendor hook
/// except for the routine at $1F1B).
pub const VENDOR_WINDOW: [u8; WINDOW_LEN] = [
    0x2F, 0xBF, 0x1B, 0xCD, 0x85, 0x12, 0xF0, 0x16, 0x8D, 0x85, 0x12, 0x8D, 0xEA, 0x13, 0xA9, 0x01,
    0x8D, 0x84, 0x12, 0x8D, 0xE9, 0x13, 0xA9, 0x02, 0x8D, 0xE8, 0x13, 0x20, 0x1F, 0xAB, 0x60, 0x6B,
    0x00, 0xBA, 0xD4, 0xBF, 0xD4, 0xA9, 0x00, 0x8D, 0x1A, 0x0F, 0x60,
];
/// Balanced: analog references follow playback and capture activity.
pub const BALANCED_WINDOW: [u8; WINDOW_LEN] = [
    0xF2, 0x18, 0x0F, 0x01, 0x1A, 0x8F, 0xA5, 0x17, 0xE2, 0x3E, 0x0F, 0x02, 0x09, 0xC2, 0x02, 0x3E,
    0x0F, 0xB2, 0x50, 0xD7, 0x57, 0x9E, 0x5F, 0x9E, 0xE6, 0xD2, 0x77, 0x1A, 0x0F, 0x80, 0xDE, 0xF2,
    0x3E, 0x0F, 0x02, 0x03, 0x20, 0x71, 0xD6, 0x1F, 0x9D, 0xD3, 0x60,
];
/// Minimum hiss: driver clamped at 0x77 when idle, settle stage on start. The immediate at
/// [`SETTLE_ADDR`] is the settle time.
pub const MIN_HISS_WINDOW: [u8; WINDOW_LEN] = [
    0xE2, 0x18, 0x0F, 0x01, 0x1C, 0xE2, 0x1A, 0x0F, 0x01, 0xF5, 0xB2, 0x0A, 0xC9, 0x17, 0x9D, 0xE2,
    0x1A, 0x0F, 0x04, 0x09, 0xC2, 0x44, 0x1A, 0x0F, 0xB2, 0x3C, 0xD7, 0x57, 0x9E, 0x5F, 0x9E, 0xDD,
    0x60, 0xD2, 0x77, 0x1A, 0x0F, 0xD2, 0x33, 0x1B, 0x0F, 0x80, 0xD2,
];
pub const SETTLE_INDEX: usize = (SETTLE_ADDR - WINDOW) as usize;

/// The routine the window holds. [`Window::Vendor`] is the vendor bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    Vendor,
    Balanced,
    MinHiss,
}

impl Window {
    pub fn image(self) -> &'static [u8; WINDOW_LEN] {
        match self {
            Window::Vendor => &VENDOR_WINDOW,
            Window::Balanced => &BALANCED_WINDOW,
            Window::MinHiss => &MIN_HISS_WINDOW,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Window::Vendor => "vendor",
            Window::Balanced => "balanced",
            Window::MinHiss => "min-hiss",
        }
    }
}

/// The window is written in two HID reports of at most 32 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    First,
    Second,
}

impl Part {
    pub const ALL: [Part; 2] = [Part::First, Part::Second];

    pub fn offset(self) -> usize {
        match self {
            Part::First => 0,
            Part::Second => 32,
        }
    }

    pub fn size(self) -> usize {
        match self {
            Part::First => 32,
            Part::Second => WINDOW_LEN - 32,
        }
    }

    pub fn addr(self) -> u16 {
        WINDOW + self.offset() as u16
    }

    pub fn of(self, image: &'static [u8; WINDOW_LEN]) -> &'static [u8] {
        &image[self.offset()..self.offset() + self.size()]
    }
}

/// Address ranges (start, length) that may ever be written. Checked on every write as a
/// second line of defence behind the patch constructors.
const WRITABLE: [(u16, usize); 5] = [
    (HOOK, 3),
    (DAC_A, 1),
    (DAC_B, 1),
    (WINDOW, WINDOW_LEN),
    (MIC_CONTROL, 1),
];

#[derive(Clone, Debug)]
enum Guard {
    /// These bytes must be at `addr`.
    Bytes { addr: u16, bytes: &'static [u8] },
    /// Every bit of `mask` must be set in the byte at `addr`.
    BitsSet { addr: u16, mask: u8 },
    /// The byte at `addr` must be in `lo..=hi`.
    ByteIn { addr: u16, lo: u8, hi: u8 },
}

#[derive(Clone, Debug)]
enum Change {
    /// Replaces `before` with `after`.
    Swap {
        before: &'static [u8],
        after: &'static [u8],
    },
    /// Replaces any byte in `lo..=hi` with `value`, which is in the same range.
    Byte { lo: u8, hi: u8, value: u8 },
}

#[derive(Clone, Debug)]
pub struct Patch {
    name: String,
    addr: u16,
    change: Change,
    guards: Vec<Guard>,
    needs_stream: bool,
}

impl Patch {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn addr(&self) -> u16 {
        self.addr
    }

    pub fn after(&self) -> &[u8] {
        match &self.change {
            Change::Swap { after, .. } => after,
            Change::Byte { value, .. } => std::slice::from_ref(value),
        }
    }

    /// True for writes that must be made while a playback stream is running: they change the
    /// hook or the routine it runs, and a stream keeps the driver from changing state
    /// meanwhile. Each carries a guard that playback has reached the codec.
    pub fn needs_stream(&self) -> bool {
        self.needs_stream
    }

    fn accepts(&self, current: &[u8]) -> bool {
        match &self.change {
            Change::Swap { before, .. } => *before == current,
            Change::Byte { lo, hi, .. } => current.len() == 1 && (*lo..=*hi).contains(&current[0]),
        }
    }

    fn is_consistent(&self) -> bool {
        match &self.change {
            Change::Swap { before, after } => before.len() == after.len() && before != after,
            Change::Byte { lo, hi, value } => lo <= value && value <= hi,
        }
    }
}

fn bytes_at(addr: u16, bytes: &'static [u8]) -> Guard {
    Guard::Bytes { addr, bytes }
}

fn hook_is_rts() -> Guard {
    bytes_at(HOOK, &[RTS])
}

fn operands_tail(window_operands: bool) -> &'static [u8; 3] {
    if window_operands {
        &WINDOW_TAIL
    } else {
        &VENDOR_TAIL
    }
}

/// A playback stream has reached the codec: the DAC is enabled and the second driver is up,
/// which is the one register that tells playback from idle in every mode.
fn playback_active() -> [Guard; 2] {
    [
        Guard::BitsSet {
            addr: DAC_ENABLE,
            mask: 0x01,
        },
        bytes_at(HP_DRIVER2, &[0x00]),
    ]
}

/// Guards that hold while the window is `window` (any allowed settle time for
/// [`Window::MinHiss`]).
fn window_guards(window: Window) -> Vec<Guard> {
    let image = window.image();
    match window {
        Window::MinHiss => vec![
            bytes_at(WINDOW, &image[..SETTLE_INDEX]),
            Guard::ByteIn {
                addr: SETTLE_ADDR,
                lo: SETTLE_MIN,
                hi: SETTLE_MAX,
            },
            bytes_at(SETTLE_ADDR + 1, &image[SETTLE_INDEX + 1..]),
        ],
        _ => vec![bytes_at(WINDOW, image)],
    }
}

fn dac_names(index: usize) -> (&'static str, u16, u16) {
    match index {
        0 => ("A", DAC_A_ROUTINE, DAC_A),
        _ => ("B", DAC_B_ROUTINE, DAC_B),
    }
}

/// Sets the DAC-hold immediate of routine `index` (0 = A, 1 = B).
pub fn dac_hold(index: usize, hold: bool) -> Patch {
    let (letter, routine, addr) = dac_names(index);
    let (before, after, word): (&'static [u8], &'static [u8], _) = if hold {
        (&[DAC_HOLD_OFF], &[DAC_HOLD_ON], "on")
    } else {
        (&[DAC_HOLD_ON], &[DAC_HOLD_OFF], "off")
    };
    Patch {
        name: format!("DAC hold {letter} {word} (${addr:04X})"),
        addr,
        change: Change::Swap { before, after },
        guards: vec![
            bytes_at(routine, &DAC_PREFIX),
            bytes_at(addr + 1, &DAC_SUFFIX),
        ],
        needs_stream: false,
    }
}

/// Hook JMP -> RTS. Refused while the analog references are off: the ROM never turns them back
/// on, and balanced mode turns them off at idle.
pub fn hook_rts(window_operands: bool) -> Patch {
    Patch {
        name: "hook30 JMP->RTS".into(),
        addr: HOOK,
        change: Change::Swap {
            before: &[JMP],
            after: &[RTS],
        },
        guards: [
            bytes_at(HOOK_OPERANDS, operands_tail(window_operands)),
            Guard::BitsSet {
                addr: REFS,
                mask: REFS_ON,
            },
        ]
        .into_iter()
        .chain(playback_active())
        .collect(),
        needs_stream: true,
    }
}

/// Hook RTS -> JMP into the routine that `window` holds; refused unless it is installed whole.
pub fn hook_jmp(window: Window) -> Patch {
    let mut guards = vec![bytes_at(
        HOOK_OPERANDS,
        operands_tail(window != Window::Vendor),
    )];
    guards.extend(window_guards(window));
    if window != Window::Vendor {
        guards.push(bytes_at(GADGET_ADDR, &GADGET));
    }
    guards.extend(playback_active());
    Patch {
        name: format!("hook30 RTS->JMP ({})", window.name()),
        addr: HOOK,
        change: Change::Swap {
            before: &[RTS],
            after: &[JMP],
        },
        guards,
        needs_stream: true,
    }
}

/// Points the hook at the vendor routine (`false`) or at the window (`true`), while the hook is RTS.
pub fn operands(window_operands: bool) -> Patch {
    let (before, after, to): (&'static [u8], &'static [u8], _) = if window_operands {
        (&VENDOR_OPERANDS, &WINDOW_OPERANDS, "window")
    } else {
        (&WINDOW_OPERANDS, &VENDOR_OPERANDS, "vendor")
    };
    Patch {
        name: format!("hook30 operands -> {to} routine"),
        addr: HOOK_OPERANDS,
        change: Change::Swap { before, after },
        guards: [hook_is_rts()]
            .into_iter()
            .chain(playback_active())
            .collect(),
        needs_stream: true,
    }
}

/// Replaces one part of the window with the same part of another image, while the hook is RTS.
/// The minimum-hiss image is the one with the default settle time.
pub fn window_part(part: Part, from: Window, to: Window) -> Patch {
    Patch {
        name: format!(
            "window ${:04X}+{}: {} -> {}",
            part.addr(),
            part.size(),
            from.name(),
            to.name()
        ),
        addr: part.addr(),
        change: Change::Swap {
            before: part.of(from.image()),
            after: part.of(to.image()),
        },
        guards: [hook_is_rts()]
            .into_iter()
            .chain(playback_active())
            .collect(),
        needs_stream: true,
    }
}

/// Changes the minimum-hiss settle time immediate in place; it is refused unless the
/// minimum-hiss routine is in the window. `units` is in 10 ms steps.
pub fn settle_time(units: u8) -> Result<Patch> {
    if !(SETTLE_MIN..=SETTLE_MAX).contains(&units) {
        return Err(Error::Refused(format!(
            "settle time {units:#04x} is outside {SETTLE_MIN:#04x}..={SETTLE_MAX:#04x}"
        )));
    }
    Ok(Patch {
        name: format!("min-hiss settle time ($1F15) = {units:#04x}"),
        addr: SETTLE_ADDR,
        change: Change::Byte {
            lo: SETTLE_MIN,
            hi: SETTLE_MAX,
            value: units,
        },
        guards: window_guards(Window::MinHiss),
        needs_stream: false,
    })
}

/// Sets $1017 to 0x38 + `step` (analog mic boost, 6 dB steps, step 0..=7).
pub fn mic_boost(step: u8) -> Result<Patch> {
    if step > MIC_MAX - MIC_MIN {
        return Err(Error::Refused(format!(
            "mic boost {step} is outside 0..={}",
            MIC_MAX - MIC_MIN
        )));
    }
    Ok(Patch {
        name: "MIC_CONTROL boost ($1017)".into(),
        addr: MIC_CONTROL,
        change: Change::Byte {
            lo: MIC_MIN,
            hi: MIC_MAX,
            value: MIC_MIN + step,
        },
        guards: Vec::new(),
        needs_stream: false,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Written,
    AlreadySet,
}

/// The only function in the app that writes to the device.
///
/// Reads the target bytes; returns `AlreadySet` if they already hold the new value;
/// refuses unless they hold an accepted before-value, the ROM identity matches and every guard
/// holds; writes; reads back and fails if the device does not hold the new bytes.
pub fn apply_patch<T: Transport + ?Sized>(t: &mut T, p: &Patch) -> Result<Outcome> {
    let len = p.after().len();
    let in_region = WRITABLE
        .iter()
        .any(|&(start, n)| p.addr >= start && p.addr as usize + len <= start as usize + n);
    if !in_region || !p.is_consistent() {
        return Err(Error::Refused(format!(
            "'{}' at ${:04X} is outside the writable patch-RAM regions",
            p.name, p.addr
        )));
    }

    let current = read_mem(t, p.addr, len)?;
    if current == p.after() {
        return Ok(Outcome::AlreadySet);
    }
    if !p.accepts(&current) {
        return Err(Error::Refused(format!(
            "'{}': ${:04X} holds {}, not an expected value",
            p.name,
            p.addr,
            hex(&current)
        )));
    }
    let rom = bytes_at(ROM_ID_ADDR, &ROM_ID);
    for g in std::iter::once(&rom).chain(&p.guards) {
        match *g {
            Guard::Bytes { addr, bytes } => {
                let got = read_mem(t, addr, bytes.len())?;
                if got != bytes {
                    return Err(Error::Refused(format!(
                        "'{}': guard ${addr:04X} holds {}, expected {}",
                        p.name,
                        hex(&got),
                        hex(bytes)
                    )));
                }
            }
            Guard::BitsSet { addr, mask } => {
                let got = read_mem(t, addr, 1)?[0];
                if got & mask != mask {
                    return Err(Error::Refused(format!(
                        "'{}': ${addr:04X} holds {got:#04x}, bits {mask:#04x} must be set",
                        p.name
                    )));
                }
            }
            Guard::ByteIn { addr, lo, hi } => {
                let got = read_mem(t, addr, 1)?[0];
                if !(lo..=hi).contains(&got) {
                    return Err(Error::Refused(format!(
                        "'{}': ${addr:04X} holds {got:#04x}, expected {lo:#04x}..={hi:#04x}",
                        p.name
                    )));
                }
            }
        }
    }

    t.send(&OutReport::write(p))?;

    let got = read_mem(t, p.addr, len)?;
    if got != p.after() {
        return Err(Error::Readback {
            patch: p.name.clone(),
            addr: p.addr,
            expected: p.after().to_vec(),
            got,
        });
    }
    Ok(Outcome::Written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{BALANCED_HEX, FakeDevice, MIN_HISS_HEX, VENDOR_HEX, bytes};
    use crate::mode::Mode;

    const WINDOWS: [Window; 3] = [Window::Vendor, Window::Balanced, Window::MinHiss];

    fn all_patches() -> Vec<Patch> {
        let mut v = Vec::new();
        for index in 0..2 {
            for hold in [false, true] {
                v.push(dac_hold(index, hold));
            }
        }
        for window_operands in [false, true] {
            v.push(hook_rts(window_operands));
            v.push(operands(window_operands));
        }
        for w in WINDOWS {
            v.push(hook_jmp(w));
        }
        for part in Part::ALL {
            for from in WINDOWS {
                for to in WINDOWS.into_iter().filter(|&t| t != from) {
                    v.push(window_part(part, from, to));
                }
            }
        }
        for units in [SETTLE_MIN, SETTLE_DEFAULT, SETTLE_MAX] {
            v.push(settle_time(units).unwrap());
        }
        for step in 0..=7 {
            v.push(mic_boost(step).unwrap());
        }
        v
    }

    fn apply(dev: &FakeDevice, p: &Patch) -> Result<Outcome> {
        apply_patch(&mut dev.clone(), p)
    }

    fn assert_refused(dev: &FakeDevice, p: &Patch) {
        let before = dev.mem();
        assert!(
            matches!(apply(dev, p), Err(Error::Refused(_))),
            "{} was not refused",
            p.name()
        );
        assert!(dev.writes().is_empty(), "{} wrote", p.name());
        assert_eq!(dev.mem(), before);
    }

    #[test]
    fn write_reports_are_cpu_ram_writes_to_allowed_addresses() {
        let allowed: [(u16, usize); 8] = [
            (0x14B1, 1),
            (0x14B2, 2),
            (0x16CF, 1),
            (0x17D0, 1),
            (0x1EFC, 32),
            (0x1F1C, 11),
            (0x1F15, 1),
            (0x1017, 1),
        ];
        for p in all_patches() {
            assert!(p.is_consistent(), "{}", p.name());
            let r = OutReport::write(&p);
            let b = r.as_bytes();
            assert_eq!(b[0], 0x04, "{}", p.name());
            assert_eq!(b[1], 0x40, "{}", p.name());
            let addr = u16::from_be_bytes([b[3], b[4]]);
            let len = b[2] as usize;
            assert!(
                allowed.contains(&(addr, len)),
                "{} -> ${addr:04X}/{len}",
                p.name()
            );
            assert_eq!(&b[5..5 + len], p.after());
            assert!(b[5 + len..].iter().all(|&x| x == 0));
        }
    }

    #[test]
    fn only_changes_to_the_hook_or_its_routine_need_a_stream() {
        for p in all_patches() {
            let hook_or_routine = (HOOK..=HOOK_OPERANDS + 1).contains(&p.addr())
                || (WINDOW..WINDOW + WINDOW_LEN as u16).contains(&p.addr());
            let in_place_settle = p.addr() == SETTLE_ADDR;
            assert_eq!(
                p.needs_stream(),
                hook_or_routine && !in_place_settle,
                "{}",
                p.name()
            );
        }
    }

    #[test]
    fn images_and_addresses_match_the_documented_firmware() {
        assert_eq!(VENDOR_WINDOW.to_vec(), bytes(VENDOR_HEX));
        assert_eq!(BALANCED_WINDOW.to_vec(), bytes(BALANCED_HEX));
        assert_eq!(MIN_HISS_WINDOW.to_vec(), bytes(MIN_HISS_HEX));
        assert_eq!(MIN_HISS_WINDOW[SETTLE_INDEX], SETTLE_DEFAULT);
        assert_eq!((SETTLE_ADDR, SETTLE_INDEX), (0x1F15, 25));
        assert_eq!((DAC_A, DAC_B), (0x16CF, 0x17D0));
        assert_eq!(Part::First.addr(), WINDOW);
        assert_eq!(
            Part::Second.addr() as usize,
            WINDOW as usize + Part::First.size()
        );
        assert_eq!(Part::First.size() + Part::Second.size(), WINDOW_LEN);
        assert!(Part::ALL.iter().all(|p| p.size() <= 32));
        for w in WINDOWS {
            let joined: Vec<u8> = Part::ALL
                .iter()
                .flat_map(|p| p.of(w.image()))
                .copied()
                .collect();
            assert_eq!(joined, w.image());
        }
    }

    #[test]
    fn writes_new_bytes_after_checks_pass() {
        let dev = FakeDevice::stock();
        assert_eq!(apply(&dev, &dac_hold(1, false)).unwrap(), Outcome::Written);
        assert_eq!(dev.writes(), vec![(0x17D0, vec![0x00])]);
        assert_eq!(dev.byte(0x17D0), 0x00);
    }

    #[test]
    fn already_set_value_is_not_rewritten() {
        let dev = FakeDevice::stock();
        assert_eq!(
            apply(&dev, &mic_boost(4).unwrap()).unwrap(),
            Outcome::AlreadySet
        );
        assert!(dev.writes().is_empty());
    }

    #[test]
    fn readback_mismatch_is_an_error() {
        let dev = FakeDevice::stock();
        dev.ignore_writes();
        let err = apply(&dev, &mic_boost(6).unwrap()).unwrap_err();
        assert!(matches!(err, Error::Readback { addr: 0x1017, .. }), "{err}");
        assert_eq!(dev.writes(), vec![(0x1017, vec![0x3E])]);
    }

    #[test]
    fn refuses_when_target_bytes_are_unexpected() {
        let dev = FakeDevice::stock();
        dev.poke(0x16CF, &[0x05]);
        assert_refused(&dev, &dac_hold(0, false));
    }

    #[test]
    fn refuses_when_dac_routine_context_differs() {
        for corrupt in [0x16C9, 0x16CE, 0x16D0, 0x16D2] {
            let dev = FakeDevice::stock();
            dev.poke(corrupt, &[dev.byte(corrupt) ^ 0xFF]);
            assert_refused(&dev, &dac_hold(0, false));
        }
    }

    #[test]
    fn refuses_mic_value_outside_allowed_range() {
        let dev = FakeDevice::stock();
        dev.poke(0x1017, &[0x07]);
        assert_refused(&dev, &mic_boost(6).unwrap());
    }

    #[test]
    fn constructors_reject_out_of_range_values() {
        assert!(mic_boost(8).is_err());
        assert!(settle_time(SETTLE_MIN - 1).is_err());
        assert!(settle_time(SETTLE_MAX + 1).is_err());
    }

    #[test]
    fn refuses_patch_outside_writable_regions() {
        let rogue = |addr, before: &'static [u8], after: &'static [u8]| Patch {
            name: "rogue".into(),
            addr,
            change: Change::Swap { before, after },
            guards: Vec::new(),
            needs_stream: false,
        };
        // an unrelated register, the byte after the hook operands, and writes that run past
        // either end of the window
        for p in [
            rogue(0x1151, &[0x00], &[0x10]),
            rogue(0x14B4, &[0x4C], &[0x60]),
            rogue(0x1F26, &[0x60, 0x00], &[0x00, 0x00]),
            rogue(0x1EFB, &[0x60, 0x2F], &[0x00, 0x00]),
        ] {
            let dev = FakeDevice::stock();
            dev.poke(0x1151, &[0x00]);
            dev.poke(0x1F27, &[0x00]);
            assert_refused(&dev, &p);
        }
    }

    #[test]
    fn refuses_routine_changes_while_the_hook_is_live() {
        // with playback shown, so that only the hook guard can refuse
        let balanced = FakeDevice::in_mode(Mode::Balanced);
        let _balanced_stream = balanced.open_active_stream().unwrap();
        assert_refused(
            &balanced,
            &window_part(Part::First, Window::Balanced, Window::MinHiss),
        );
        assert_refused(
            &balanced,
            &window_part(Part::Second, Window::Balanced, Window::Vendor),
        );
        assert_refused(&balanced, &operands(false));
        let stock = FakeDevice::in_mode(Mode::Stock);
        let _stock_stream = stock.open_active_stream().unwrap();
        assert_refused(
            &stock,
            &window_part(Part::First, Window::Vendor, Window::Balanced),
        );
        assert_refused(&stock, &operands(true));
    }

    #[test]
    fn refuses_to_go_live_without_a_whole_routine() {
        for w in WINDOWS {
            for other in WINDOWS.into_iter().filter(|&o| o != w) {
                let dev = FakeDevice::in_mode(Mode::Quiet);
                let operands: &[u8] = if w == Window::Vendor {
                    &VENDOR_OPERANDS
                } else {
                    &WINDOW_OPERANDS
                };
                dev.poke(HOOK_OPERANDS, operands);
                dev.poke(Part::First.addr(), Part::First.of(w.image()));
                dev.poke(Part::Second.addr(), Part::Second.of(other.image()));
                let _stream = dev.open_active_stream().unwrap();
                assert_refused(&dev, &hook_jmp(w));

                dev.poke(Part::First.addr(), Part::First.of(other.image()));
                assert_refused(&dev, &hook_jmp(w));
            }
        }
    }

    #[test]
    fn refuses_to_go_live_with_the_wrong_operands() {
        let dev = FakeDevice::in_mode(Mode::Quiet);
        dev.poke(0x1EFC, &bytes(BALANCED_HEX));
        let _stream = dev.open_active_stream().unwrap();
        assert_refused(&dev, &hook_jmp(Window::Balanced));
        let dev = FakeDevice::in_mode(Mode::Quiet);
        dev.poke(HOOK_OPERANDS, &WINDOW_OPERANDS);
        let _stream = dev.open_active_stream().unwrap();
        assert_refused(&dev, &hook_jmp(Window::Vendor));
    }

    #[test]
    fn min_hiss_goes_live_with_any_settle_time() {
        let dev = FakeDevice::in_mode(Mode::Quiet);
        dev.poke(HOOK_OPERANDS, &WINDOW_OPERANDS);
        dev.poke(WINDOW, &bytes(MIN_HISS_HEX));
        dev.poke(SETTLE_ADDR, &[0x20]);
        dev.settle();
        let _stream = dev.open_active_stream().unwrap();
        assert_eq!(
            apply(&dev, &hook_jmp(Window::MinHiss)).unwrap(),
            Outcome::Written
        );
        assert_eq!(dev.writes(), vec![(0x14B1, vec![JMP])]);
    }

    #[test]
    fn refuses_to_turn_the_hook_off_while_the_references_are_off() {
        // balanced idle: the references are off, and only balanced mode turns them back on
        let dev = FakeDevice::in_mode(Mode::Balanced);
        assert_eq!(dev.byte(REFS) & REFS_ON, 0);
        assert_refused(&dev, &hook_rts(true));

        // the same switch is accepted once a stream has brought the references up
        let _stream = dev.open_active_stream().unwrap();
        assert_eq!(dev.byte(REFS) & REFS_ON, REFS_ON);
        assert_eq!(apply(&dev, &hook_rts(true)).unwrap(), Outcome::Written);
        assert_eq!(dev.byte(HOOK), RTS);

        // playback shown but the references off: only the references guard refuses
        let dev = FakeDevice::in_mode(Mode::Balanced);
        let _stream = dev.open_active_stream().unwrap();
        dev.poke(REFS, &[0x00]);
        assert_refused(&dev, &hook_rts(true));
    }

    #[test]
    fn refuses_to_turn_the_hook_off_with_the_wrong_operands() {
        let dev = FakeDevice::in_mode(Mode::Stock);
        let _stock_stream = dev.open_active_stream().unwrap();
        assert_refused(&dev, &hook_rts(true));
        let dev = FakeDevice::in_mode(Mode::Balanced);
        let _stream = dev.open_active_stream().unwrap();
        assert_refused(&dev, &hook_rts(false));
    }

    /// Cases where each stream patch would be accepted but for the stream.
    fn stream_cases() -> Vec<(FakeDevice, Patch)> {
        vec![
            (FakeDevice::in_mode(Mode::Stock), hook_rts(false)),
            (FakeDevice::in_mode(Mode::Quiet), operands(true)),
            (
                FakeDevice::in_mode(Mode::Quiet),
                window_part(Part::First, Window::Vendor, Window::Balanced),
            ),
            (
                FakeDevice::in_mode(Mode::Quiet),
                window_part(Part::Second, Window::Vendor, Window::Balanced),
            ),
            (FakeDevice::in_mode(Mode::Quiet), hook_jmp(Window::Vendor)),
            (quiet_with(BALANCED_HEX), hook_jmp(Window::Balanced)),
            (quiet_with(MIN_HISS_HEX), hook_jmp(Window::MinHiss)),
        ]
    }

    /// Hook RTS, operands and window as if a routine were installed whole.
    fn quiet_with(window: &str) -> FakeDevice {
        let dev = FakeDevice::in_mode(Mode::Quiet);
        dev.poke(HOOK_OPERANDS, &WINDOW_OPERANDS);
        dev.poke(WINDOW, &bytes(window));
        dev
    }

    #[test]
    fn stream_patches_are_refused_until_the_codec_shows_playback() {
        for (dev, p) in stream_cases() {
            assert!(p.needs_stream(), "{}", p.name());
            assert_refused(&dev, &p);
        }
        for (dev, p) in stream_cases() {
            let _stream = dev.open_stream().unwrap();
            assert!(!dev.codec_sees_stream());
            assert_refused(&dev, &p);
        }
        for (dev, p) in stream_cases() {
            let _stream = dev.open_active_stream().unwrap();
            assert_eq!(apply(&dev, &p).unwrap(), Outcome::Written, "{}", p.name());
        }
    }

    #[test]
    fn stream_patches_need_both_playback_registers() {
        // $0F18 bit 0 is the DAC; $0F1B is the one register that differs from idle in stock
        for (addr, idle) in [(DAC_ENABLE, 0x02), (HP_DRIVER2, 0x33)] {
            for (dev, p) in stream_cases() {
                let _stream = dev.open_active_stream().unwrap();
                dev.poke(addr, &[idle]);
                assert_refused(&dev, &p);
            }
        }
    }

    #[test]
    fn nothing_is_written_on_an_unknown_rom() {
        for i in 0..ROM_ID.len() {
            let addr = ROM_ID_ADDR + i as u16;
            for (mode, p) in [
                (Mode::Stock, dac_hold(0, false)),
                (Mode::Stock, mic_boost(6).unwrap()),
                (Mode::MinHiss, settle_time(0x20).unwrap()),
            ] {
                let dev = FakeDevice::in_mode(mode);
                dev.poke(addr, &[dev.byte(addr) ^ 0x01]);
                assert_refused(&dev, &p);
            }
        }
    }

    #[test]
    fn window_routines_go_live_only_with_the_gadget_in_place() {
        for (w, hex) in [
            (Window::Balanced, BALANCED_HEX),
            (Window::MinHiss, MIN_HISS_HEX),
        ] {
            for offset in 0..GADGET.len() as u16 {
                let dev = quiet_with(hex);
                let addr = GADGET_ADDR + offset;
                dev.poke(addr, &[dev.byte(addr) ^ 0xFF]);
                let _stream = dev.open_active_stream().unwrap();
                assert_refused(&dev, &hook_jmp(w));
            }
        }
        // the vendor routine does not leave through the gadget
        let dev = FakeDevice::in_mode(Mode::Quiet);
        dev.poke(GADGET_ADDR, &[0x00]);
        let _stream = dev.open_active_stream().unwrap();
        assert_eq!(
            apply(&dev, &hook_jmp(Window::Vendor)).unwrap(),
            Outcome::Written
        );
    }

    #[test]
    fn min_hiss_goes_live_only_with_a_settle_time_in_range() {
        for (settle, live) in [
            (0x00, false),
            (0x01, false),
            (SETTLE_MIN, true),
            (SETTLE_MAX, true),
            (SETTLE_MAX + 1, false),
            (0xFF, false),
        ] {
            let dev = quiet_with(MIN_HISS_HEX);
            dev.poke(SETTLE_ADDR, &[settle]);
            let _stream = dev.open_active_stream().unwrap();
            if live {
                assert_eq!(
                    apply(&dev, &hook_jmp(Window::MinHiss)).unwrap(),
                    Outcome::Written,
                    "{settle:#04x}"
                );
            } else {
                assert_refused(&dev, &hook_jmp(Window::MinHiss));
            }
        }
    }

    #[test]
    fn settle_time_is_changed_in_place_only_in_the_min_hiss_routine() {
        let dev = FakeDevice::in_mode(Mode::MinHiss);
        assert_eq!(
            apply(&dev, &settle_time(0x20).unwrap()).unwrap(),
            Outcome::Written
        );
        assert_eq!(dev.writes(), vec![(0x1F15, vec![0x20])]);
        assert_eq!(dev.byte(HOOK), JMP);

        for corrupt in [0x1EFC, 0x1F14, 0x1F16, 0x1F26] {
            let dev = FakeDevice::in_mode(Mode::MinHiss);
            dev.poke(corrupt, &[dev.byte(corrupt) ^ 0xFF]);
            assert_refused(&dev, &settle_time(0x20).unwrap());
        }
        for mode in [Mode::Stock, Mode::Quiet, Mode::Balanced] {
            let dev = FakeDevice::in_mode(mode);
            dev.poke(SETTLE_ADDR, &[0x3C]);
            assert_refused(&dev, &settle_time(0x20).unwrap());
        }
    }
}
