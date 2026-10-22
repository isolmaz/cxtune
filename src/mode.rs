//! Output modes, device state readout, and planning the patch sequence that moves the
//! device from its current state to a mode.

use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::patches::{
    self, DAC_HOLD_OFF, DAC_HOLD_ON, DAC_PREFIX, DAC_SUFFIX, JMP, MIC_MAX, MIC_MIN, Outcome, Part,
    Patch, RTS, SETTLE_DEFAULT, SETTLE_INDEX, SETTLE_MAX, SETTLE_MIN, VENDOR_OPERANDS,
    WINDOW_OPERANDS, Window, apply_patch,
};
use crate::protocol::{Transport, read_mem};
use crate::{Error, Result, hex};

/// $0F18: DAC channel enables; bit 0 is set while a playback stream is running.
pub const DAC_ENABLE: u16 = 0x0F18;
/// $0F1A: headphone driver power state; 0x00 is fully active.
pub const HP_DRIVER: u16 = 0x0F1A;

/// How long a freshly opened playback stream may take to bring the driver fully up.
pub const READY_TIMEOUT: Duration = Duration::from_secs(15);
const READY_POLL: Duration = Duration::from_millis(50);

const SETTLE_STEP_MS: u16 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Vendor behaviour: DACs and headphone driver always powered. Constant hiss, no pops.
    Stock,
    /// ROM default idle power-down: the driver parks at 0x33 when idle. Low pop, some idle hiss.
    Quiet,
    /// Driver clamped at 0x77 and analog references off when idle; ROM power-up order on start.
    Balanced,
    /// Driver clamped at 0x77 when idle; on start it first holds 0x33 for the settle time. Loud
    /// start pop.
    MinHiss,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Stock, Mode::Quiet, Mode::Balanced, Mode::MinHiss];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Stock => "stock",
            Mode::Quiet => "quiet",
            Mode::Balanced => "balanced",
            Mode::MinHiss => "min-hiss",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.name() == s)
    }

    fn layout(self) -> Layout {
        let (hold, jmp, window_operands, window) = match self {
            Mode::Stock => (DAC_HOLD_ON, true, false, Window::Vendor),
            Mode::Quiet => (DAC_HOLD_OFF, false, false, Window::Vendor),
            Mode::Balanced => (DAC_HOLD_OFF, true, true, Window::Balanced),
            Mode::MinHiss => (DAC_HOLD_OFF, true, true, Window::MinHiss),
        };
        Layout {
            hold: [hold; 2],
            jmp,
            window_operands,
            parts: [window; 2],
        }
    }
}

/// Every byte group the planner changes. A mode is one layout; a device caught half-way
/// through a change holds some other layout that the planner can still leave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    /// DAC-hold immediates of routines A and B.
    hold: [u8; 2],
    /// The hook opcode is JMP (live) rather than RTS.
    jmp: bool,
    /// The hook operands point at the window instead of the vendor routine.
    window_operands: bool,
    /// What the two parts of the window hold.
    parts: [Window; 2],
}

/// Raw bytes of every location the app knows about.
#[derive(Clone, Debug)]
pub struct State {
    /// $14B1..$14B4: hook30 opcode and operands, then the next hook's opcode.
    pub hook: [u8; 4],
    /// $16C9..$16D2 and $17CA..$17D3: the DAC-hold routines.
    pub dac_routines: [[u8; 10]; 2],
    /// $1EFC..$1F26: vendor bytes or one of the routines.
    pub window: [u8; patches::WINDOW_LEN],
    /// $1017 MIC_CONTROL.
    pub mic_control: u8,
    /// $0F18 DAC enable bits 0/1 (read-only).
    pub dac_enable: u8,
    /// $0F1A headphone driver: 0x00 active, 0x33 parked, 0x77 clamped (read-only).
    pub hp_driver: u8,
    /// $0F1B (read-only).
    pub reg_0f1b: u8,
    /// $0F2C (read-only).
    pub reg_0f2c: u8,
    /// $0F3E analog references (read-only).
    pub refs: u8,
}

pub fn read_state<T: Transport + ?Sized>(t: &mut T) -> Result<State> {
    let mut r = |addr: u16, len: usize| read_mem(t, addr, len);
    let hook = r(patches::HOOK, 4)?;
    let dac_a = r(patches::DAC_A_ROUTINE, 10)?;
    let dac_b = r(patches::DAC_B_ROUTINE, 10)?;
    let window = r(patches::WINDOW, patches::WINDOW_LEN)?;
    let mic = r(patches::MIC_CONTROL, 1)?;
    let dac_enable = r(DAC_ENABLE, 1)?;
    let hp = r(HP_DRIVER, 2)?;
    let reg_0f2c = r(0x0F2C, 1)?;
    let refs = r(patches::REFS, 1)?;
    Ok(State {
        hook: hook.try_into().unwrap(),
        dac_routines: [dac_a.try_into().unwrap(), dac_b.try_into().unwrap()],
        window: window.try_into().unwrap(),
        mic_control: mic[0],
        dac_enable: dac_enable[0],
        hp_driver: hp[0],
        reg_0f1b: hp[1],
        reg_0f2c: reg_0f2c[0],
        refs: refs[0],
    })
}

fn part_matches(window: Window, part: Part, got: &[u8]) -> bool {
    let want = part.of(window.image());
    if window == Window::MinHiss && part == Part::First {
        got[..SETTLE_INDEX] == want[..SETTLE_INDEX]
            && got[SETTLE_INDEX + 1..] == want[SETTLE_INDEX + 1..]
            && (SETTLE_MIN..=SETTLE_MAX).contains(&got[SETTLE_INDEX])
    } else {
        got == want
    }
}

impl State {
    /// What each part of the window holds, or `None` if either matches no known routine.
    /// The minimum-hiss routine matches with any settle time in the allowed range.
    pub fn parts(&self) -> Option<[Window; 2]> {
        let find = |part: Part| {
            let got = &self.window[part.offset()..part.offset() + part.size()];
            [Window::Vendor, Window::Balanced, Window::MinHiss]
                .into_iter()
                .find(|&w| part_matches(w, part, got))
        };
        Some([find(Part::First)?, find(Part::Second)?])
    }

    /// The minimum-hiss settle time in 10 ms units, if that routine is in the window.
    pub fn settle_units(&self) -> Option<u8> {
        (self.parts()?[0] == Window::MinHiss).then(|| self.window[SETTLE_INDEX])
    }

    pub fn settle_ms(&self) -> Option<u16> {
        self.settle_units().map(|u| u as u16 * SETTLE_STEP_MS)
    }

    /// The DAC-hold immediate of each routine, or `None` if a routine does not match.
    pub fn dac_hold(&self) -> [Option<u8>; 2] {
        self.dac_routines.map(|r| {
            let ok = r[..6] == DAC_PREFIX
                && r[7..] == DAC_SUFFIX
                && (r[6] == DAC_HOLD_ON || r[6] == DAC_HOLD_OFF);
            ok.then_some(r[6])
        })
    }

    /// The hook's opcode and operands as a pair (live, points at the window), or `None` if the
    /// bytes at $14B1..$14B4 are neither the vendor hook nor ours.
    pub fn hook_config(&self) -> Option<(bool, bool)> {
        let [op, lo, hi, next] = self.hook;
        let jmp = match op {
            JMP => true,
            RTS => false,
            _ => return None,
        };
        let window_operands = match [lo, hi] {
            VENDOR_OPERANDS => false,
            WINDOW_OPERANDS => true,
            _ => return None,
        };
        (next == JMP).then_some((jmp, window_operands))
    }

    fn layout(&self) -> Result<Layout> {
        let (jmp, window_operands) = self.hook_config().ok_or_else(|| {
            Error::Refused(format!(
                "unrecognised hook bytes at $14B1: {}",
                hex(&self.hook)
            ))
        })?;
        let parts = self.parts().ok_or_else(|| {
            Error::Refused(format!(
                "unrecognised routine bytes at $1EFC: {}",
                hex(&self.window)
            ))
        })?;
        let [Some(a), Some(b)] = self.dac_hold() else {
            return Err(Error::Refused(format!(
                "unrecognised DAC-hold routine: {} / {}",
                hex(&self.dac_routines[0]),
                hex(&self.dac_routines[1])
            )));
        };
        // A live hook must run a whole routine: the vendor one, or one of ours installed in
        // both parts of the window.
        let whole = if window_operands {
            parts[0] == parts[1] && parts[0] != Window::Vendor
        } else {
            parts == [Window::Vendor; 2]
        };
        if jmp && !whole {
            return Err(Error::Refused(format!(
                "the live hook at $14B1 ({}) does not lead to a whole routine ($1EFC {})",
                hex(&self.hook),
                hex(&self.window)
            )));
        }
        Ok(Layout {
            hold: [a, b],
            jmp,
            window_operands,
            parts,
        })
    }

    /// The mode the device is in, or `None` for a half-changed or unrecognised state.
    pub fn mode(&self) -> Option<Mode> {
        let layout = self.layout().ok()?;
        Mode::ALL.into_iter().find(|m| m.layout() == layout)
    }

    /// Mic boost step 0..=7, or `None` if $1017 is outside 0x38..=0x3F.
    pub fn mic_boost(&self) -> Option<u8> {
        (MIC_MIN..=MIC_MAX)
            .contains(&self.mic_control)
            .then(|| self.mic_control - MIC_MIN)
    }
}

/// The ordered patches that move `state` to `target`, or an error without writing anything if
/// the state is not recognised.
///
/// The hook is turned to RTS before the routine or its operands change and to JMP last. When
/// the DACs are released, that happens before the hook changes; when they are held again, after.
/// A changed minimum-hiss settle time is put back to the default before the routine is replaced.
pub fn plan(state: &State, target: Mode) -> Result<Vec<Patch>> {
    let from = state.layout()?;
    let to = target.layout();

    let operands_change = from.window_operands != to.window_operands;
    let parts_change = from.parts != to.parts;
    let leave = from.jmp && (operands_change || parts_change || !to.jmp);
    let enter = to.jmp && (leave || !from.jmp);

    let mut hook = Vec::new();
    if let Some(units) = state.settle_units()
        && units != SETTLE_DEFAULT
        && from.parts[0] != to.parts[0]
    {
        hook.push(patches::settle_time(SETTLE_DEFAULT)?);
    }
    if leave {
        hook.push(patches::hook_rts(from.window_operands));
    }
    let parts = Part::ALL
        .into_iter()
        .zip(from.parts.into_iter().zip(to.parts))
        .filter(|(_, (f, t))| f != t)
        .map(|(part, (f, t))| patches::window_part(part, f, t));
    let operands = operands_change.then(|| patches::operands(to.window_operands));
    if to.window_operands {
        hook.extend(parts);
        hook.extend(operands);
    } else {
        hook.extend(operands);
        hook.extend(parts);
    }
    if enter {
        hook.push(patches::hook_jmp(to.parts[0]));
    }

    let dac = (0..2)
        .filter(|&i| from.hold[i] != to.hold[i])
        .map(|i| patches::dac_hold(i, to.hold[i] == DAC_HOLD_ON));
    Ok(if to.hold[0] == DAC_HOLD_OFF {
        dac.chain(hook).collect()
    } else {
        hook.into_iter().chain(dac).collect()
    })
}

/// Moves the device to `target` and returns the number of byte-changing writes made.
///
/// Changes to the hook or the routine it runs are made while the stream that `open_stream`
/// opens is running: it must play digital silence on the dongle's output. The stream keeps
/// the headphone driver from changing state while the hook is briefly off, and brings the
/// analog references up before balanced mode is left.
pub fn apply_mode<T: Transport + ?Sized, S>(
    t: &mut T,
    target: Mode,
    open_stream: impl FnOnce() -> Result<S>,
) -> Result<usize> {
    run(t, target, open_stream, READY_TIMEOUT)
}

fn run<T: Transport + ?Sized, S>(
    t: &mut T,
    target: Mode,
    open_stream: impl FnOnce() -> Result<S>,
    ready_timeout: Duration,
) -> Result<usize> {
    let steps = plan(&read_state(t)?, target)?;
    let _stream = if steps.iter().any(Patch::needs_stream) {
        let stream = open_stream()?;
        wait_ready(t, ready_timeout)?;
        Some(stream)
    } else {
        None
    };
    let mut written = 0;
    for p in &steps {
        if apply_patch(t, p)? == Outcome::Written {
            written += 1;
        }
    }
    Ok(written)
}

/// Waits until playback is running with the driver fully on and the analog references up.
fn wait_ready<T: Transport + ?Sized>(t: &mut T, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let enable = read_mem(t, DAC_ENABLE, 1)?[0];
        let driver = read_mem(t, HP_DRIVER, 1)?[0];
        let refs = read_mem(t, patches::REFS, 1)?[0];
        if enable & 1 != 0 && driver == 0x00 && refs & patches::REFS_ON != 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Device(format!(
                "the dongle did not reach the active playback state within {} s \
                 ($0F18={enable:#04x} $0F1A={driver:#04x} $0F3E={refs:#04x}); nothing was written. \
                 Check that no other program holds the output exclusively; if the references \
                 ($0F3E bit 1) stay off, replug the dongle",
                timeout.as_secs()
            )));
        }
        sleep(READY_POLL);
    }
}

/// Converts a settle time in milliseconds to 10 ms units.
pub fn settle_units(ms: u16) -> Result<u8> {
    let units = ms / SETTLE_STEP_MS;
    if !ms.is_multiple_of(SETTLE_STEP_MS)
        || !(SETTLE_MIN as u16..=SETTLE_MAX as u16).contains(&units)
    {
        return Err(Error::Refused(format!(
            "settle time must be {}..={} ms in steps of {SETTLE_STEP_MS} ms, got {ms}",
            SETTLE_MIN as u16 * SETTLE_STEP_MS,
            SETTLE_MAX as u16 * SETTLE_STEP_MS
        )));
    }
    Ok(units as u8)
}

/// Sets the minimum-hiss settle time (milliseconds). Returns true if a byte was written.
pub fn tune_min_hiss<T: Transport + ?Sized>(t: &mut T, ms: u16) -> Result<bool> {
    let units = settle_units(ms)?;
    if read_state(t)?.settle_units().is_none() {
        return Err(Error::Refused(
            "the min-hiss routine is not installed; apply the min-hiss mode first".into(),
        ));
    }
    Ok(apply_patch(t, &patches::settle_time(units)?)? == Outcome::Written)
}

/// Sets the analog mic boost step (0..=7, $1017 = 0x38 + step). Returns true if written.
pub fn set_mic_boost<T: Transport + ?Sized>(t: &mut T, step: u8) -> Result<bool> {
    Ok(apply_patch(t, &patches::mic_boost(step)?)? == Outcome::Written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{BALANCED_HEX, FakeDevice, MIN_HISS_HEX, VENDOR_HEX, bytes};

    fn apply(dev: &FakeDevice, target: Mode) -> Result<usize> {
        apply_mode(&mut dev.clone(), target, || dev.open_stream())
    }

    fn state(dev: &FakeDevice) -> State {
        read_state(&mut dev.clone()).unwrap()
    }

    fn w(addr: u16, hex: &str) -> (u16, Vec<u8>) {
        (addr, bytes(hex))
    }

    /// The two window parts of an image, as hex.
    fn part1(image: &str) -> &str {
        &image[..64]
    }
    fn part2(image: &str) -> &str {
        &image[64..]
    }

    fn writes_of(from: Mode, to: Mode) -> Vec<(u16, Vec<u8>)> {
        let dev = FakeDevice::in_mode(from);
        apply(&dev, to).unwrap();
        dev.writes()
    }

    #[test]
    fn each_mode_reads_back_as_itself() {
        for mode in Mode::ALL {
            let dev = FakeDevice::in_mode(mode);
            let s = state(&dev);
            assert_eq!(s.mode(), Some(mode), "{mode:?}");
            assert_eq!(s.mic_boost(), Some(4));
            let settle = (mode == Mode::MinHiss).then_some(600);
            assert_eq!(s.settle_ms(), settle, "{mode:?}");
            assert!(dev.writes().is_empty());
        }
    }

    #[test]
    fn every_mode_reaches_every_mode() {
        for from in Mode::ALL {
            for to in Mode::ALL {
                let dev = FakeDevice::in_mode(from);
                apply(&dev, to).unwrap_or_else(|e| panic!("{from:?} -> {to:?}: {e}"));
                assert_eq!(
                    dev.mem(),
                    FakeDevice::in_mode(to).mem(),
                    "{from:?} -> {to:?}"
                );
                assert_eq!(state(&dev).mode(), Some(to));
                assert!(!dev.stream_is_open());
                // one stream covers the whole change, and none is opened for no change
                assert_eq!(
                    dev.streams_opened(),
                    usize::from(from != to),
                    "{from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn applying_a_mode_twice_writes_nothing_the_second_time() {
        for mode in Mode::ALL {
            let dev = FakeDevice::stock();
            apply(&dev, mode).unwrap();
            dev.clear_writes();
            let opened = dev.streams_opened();
            assert_eq!(apply(&dev, mode).unwrap(), 0);
            assert!(dev.writes().is_empty());
            assert_eq!(dev.streams_opened(), opened);
        }
    }

    #[test]
    fn writes_go_in_a_safe_order() {
        let vendor = VENDOR_HEX;
        let balanced = BALANCED_HEX;
        let min_hiss = MIN_HISS_HEX;
        let dacs_off = [w(0x16CF, "00"), w(0x17D0, "00")];
        let dacs_on = [w(0x16CF, "03"), w(0x17D0, "03")];

        assert_eq!(
            writes_of(Mode::Stock, Mode::Quiet),
            [dacs_off.to_vec(), vec![w(0x14B1, "60")]].concat()
        );
        assert_eq!(
            writes_of(Mode::Quiet, Mode::Stock),
            [vec![w(0x14B1, "4c")], dacs_on.to_vec()].concat()
        );
        // install: routine parts, operands, opcode last
        assert_eq!(
            writes_of(Mode::Stock, Mode::Balanced),
            [
                dacs_off.to_vec(),
                vec![
                    w(0x14B1, "60"),
                    w(0x1EFC, part1(balanced)),
                    w(0x1F1C, part2(balanced)),
                    w(0x14B2, "fc1e"),
                    w(0x14B1, "4c"),
                ]
            ]
            .concat()
        );
        // removal: opcode first, operands, routine parts, then the vendor hook is live again
        assert_eq!(
            writes_of(Mode::Balanced, Mode::Stock),
            [
                vec![
                    w(0x14B1, "60"),
                    w(0x14B2, "1b1f"),
                    w(0x1EFC, part1(vendor)),
                    w(0x1F1C, part2(vendor)),
                    w(0x14B1, "4c"),
                ],
                dacs_on.to_vec()
            ]
            .concat()
        );
        assert_eq!(
            writes_of(Mode::Balanced, Mode::MinHiss),
            vec![
                w(0x14B1, "60"),
                w(0x1EFC, part1(min_hiss)),
                w(0x1F1C, part2(min_hiss)),
                w(0x14B1, "4c"),
            ]
        );
        assert_eq!(
            writes_of(Mode::Quiet, Mode::Balanced),
            vec![
                w(0x1EFC, part1(balanced)),
                w(0x1F1C, part2(balanced)),
                w(0x14B2, "fc1e"),
                w(0x14B1, "4c"),
            ]
        );
        assert_eq!(
            writes_of(Mode::MinHiss, Mode::Quiet),
            vec![
                w(0x14B1, "60"),
                w(0x14B2, "1b1f"),
                w(0x1EFC, part1(vendor)),
                w(0x1F1C, part2(vendor)),
            ]
        );
    }

    #[test]
    fn leaving_balanced_runs_with_the_references_up() {
        // The fake panics if the hook is turned off while the references are off; balanced idle
        // has them off, so the stream must have raised them before the first write.
        for to in [Mode::Stock, Mode::Quiet, Mode::MinHiss] {
            let dev = FakeDevice::in_mode(Mode::Balanced);
            assert_eq!(dev.byte(patches::REFS) & patches::REFS_ON, 0);
            apply(&dev, to).unwrap();
            assert_eq!(dev.byte(patches::REFS) & patches::REFS_ON, patches::REFS_ON);
        }
    }

    #[test]
    fn an_interrupted_change_can_be_completed_to_any_mode() {
        for from in Mode::ALL {
            for tuned in [false, true] {
                if tuned && from != Mode::MinHiss {
                    continue;
                }
                let start = || {
                    let dev = FakeDevice::in_mode(from);
                    if tuned {
                        tune_min_hiss(&mut dev.clone(), 300).unwrap();
                    }
                    dev.clear_writes();
                    dev
                };
                for to in Mode::ALL {
                    let probe = start();
                    let total = apply(&probe, to).unwrap();
                    for done in 0..total {
                        for next in Mode::ALL {
                            let label = format!(
                                "{from:?} tuned={tuned} -> {to:?} cut after {done} -> {next:?}"
                            );
                            let dev = start();
                            dev.fail_after_writes(done);
                            let err = apply(&dev, to).expect_err(&label);
                            assert!(matches!(err, Error::Device(_)), "{label}: {err}");
                            assert_eq!(dev.writes().len(), done, "{label}");
                            assert!(!dev.stream_is_open(), "{label}");
                            dev.clear_failure();
                            apply(&dev, next).unwrap_or_else(|e| panic!("{label}: {e}"));
                            let mut expected = FakeDevice::in_mode(next).mem();
                            if tuned && next == Mode::MinHiss && done == 0 {
                                // nothing was written, so the settle time is still tuned
                                expected[0x1F15] = 0x1E;
                            }
                            assert_eq!(dev.mem(), expected, "{label}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_half_changed_device_is_not_a_mode_but_can_be_planned_from() {
        let dev = FakeDevice::in_mode(Mode::Quiet);
        dev.poke(0x1EFC, &bytes(BALANCED_HEX));
        dev.poke(0x14B2, &[0xFC, 0x1E]);
        dev.settle();
        assert_eq!(state(&dev).mode(), None);
        // only the hook is left to turn on
        assert_eq!(plan(&state(&dev), Mode::Balanced).unwrap().len(), 1);
        apply(&dev, Mode::Balanced).unwrap();
        assert_eq!(dev.writes(), vec![w(0x14B1, "4c")]);
    }

    #[test]
    fn dac_only_changes_need_no_stream() {
        // stock whose DAC holds were released but whose hook is still the vendor one
        let dev = FakeDevice::in_mode(Mode::Stock);
        dev.poke(0x16CF, &[0x00]);
        dev.poke(0x17D0, &[0x00]);
        dev.settle();
        apply(&dev, Mode::Stock).unwrap();
        assert_eq!(dev.writes(), vec![w(0x16CF, "03"), w(0x17D0, "03")]);
        assert_eq!(dev.streams_opened(), 0);
        assert_eq!(dev.mem(), FakeDevice::in_mode(Mode::Stock).mem());
    }

    #[test]
    fn unrecognised_states_are_refused_without_writes_or_a_stream() {
        let regions: [(u16, usize); 4] = [(0x14B1, 4), (0x1EFC, 43), (0x16C9, 10), (0x17CA, 10)];
        for from in Mode::ALL {
            for (addr, len) in regions {
                for a in addr..addr + len as u16 {
                    let dev = FakeDevice::in_mode(from);
                    // a settle time outside its range is the only change to that byte that
                    // the min-hiss routine does not accept
                    let bad = if a == 0x1F15 {
                        0x00
                    } else {
                        dev.byte(a) ^ 0xFF
                    };
                    dev.poke(a, &[bad]);
                    for to in Mode::ALL {
                        let err = apply(&dev, to).unwrap_err();
                        assert!(
                            matches!(err, Error::Refused(_)),
                            "{from:?} with ${a:04X}={bad:02x} -> {to:?}: {err}"
                        );
                        assert!(dev.writes().is_empty());
                        assert_eq!(dev.streams_opened(), 0);
                    }
                    assert_eq!(state(&dev).mode(), None);
                }
            }
        }
    }

    #[test]
    fn a_live_hook_without_a_whole_routine_is_refused() {
        let cases: [(Mode, u16, Vec<u8>); 4] = [
            // vendor hook with operands pointing at the window
            (Mode::Stock, 0x14B2, vec![0xFC, 0x1E]),
            // window hook with operands pointing at the vendor routine
            (Mode::Balanced, 0x14B2, vec![0x1B, 0x1F]),
            // window hook, vendor bytes in the second part
            (Mode::Balanced, 0x1F1C, bytes(VENDOR_HEX)[32..].to_vec()),
            // window hook, balanced bytes in the first part of the min-hiss routine
            (Mode::MinHiss, 0x1EFC, bytes(BALANCED_HEX)[..32].to_vec()),
        ];
        for (mode, addr, data) in cases {
            let dev = FakeDevice::in_mode(mode);
            dev.poke(addr, &data);
            for to in Mode::ALL {
                assert!(
                    matches!(apply(&dev, to), Err(Error::Refused(_))),
                    "{mode:?} with {data:02x?} at ${addr:04X} -> {to:?}"
                );
            }
            assert!(dev.writes().is_empty());
            assert_eq!(dev.streams_opened(), 0);
        }
    }

    #[test]
    fn nothing_is_written_when_the_stream_cannot_be_opened() {
        let dev = FakeDevice::stock();
        dev.remove_endpoint();
        assert!(matches!(apply(&dev, Mode::Balanced), Err(Error::Device(_))));
        assert!(dev.writes().is_empty());
        // a change that needs no stream is still possible
        let dev = FakeDevice::in_mode(Mode::Stock);
        dev.remove_endpoint();
        dev.poke(0x16CF, &[0x00]);
        dev.poke(0x17D0, &[0x00]);
        dev.settle();
        apply(&dev, Mode::Stock).unwrap();
    }

    /// Balanced mode's hook switched off while it had the references off: the state a replug
    /// is needed to leave, because nothing turns the references back on.
    fn references_stuck_off() -> FakeDevice {
        let dev = FakeDevice::in_mode(Mode::Balanced);
        dev.poke(0x14B1, &[0x60]);
        dev.settle();
        assert_eq!(dev.byte(patches::REFS) & patches::REFS_ON, 0);
        dev
    }

    #[test]
    fn nothing_is_written_while_the_references_cannot_be_raised() {
        for to in Mode::ALL {
            let dev = references_stuck_off();
            let err = run(
                &mut dev.clone(),
                to,
                || dev.open_stream(),
                Duration::from_millis(120),
            )
            .unwrap_err();
            assert!(matches!(err, Error::Device(_)), "{to:?}: {err}");
            assert!(dev.writes().is_empty(), "{to:?}");
            assert!(!dev.stream_is_open());
        }
    }

    #[test]
    fn the_wait_names_the_registers_when_the_device_never_becomes_active() {
        let dev = references_stuck_off();
        let err = run(
            &mut dev.clone(),
            Mode::Quiet,
            || dev.open_stream(),
            Duration::from_millis(60),
        )
        .unwrap_err();
        let Error::Device(message) = err else {
            panic!("{err}")
        };
        assert!(message.contains("$0F3E=0x00"), "{message}");
    }

    #[test]
    fn settle_time_converts_from_milliseconds() {
        assert_eq!(settle_units(20).unwrap(), 2);
        assert_eq!(settle_units(600).unwrap(), 0x3C);
        assert_eq!(settle_units(2000).unwrap(), 0xC8);
        for bad in [0, 10, 15, 605, 2010, 2500, u16::MAX] {
            assert!(matches!(settle_units(bad), Err(Error::Refused(_))), "{bad}");
        }
    }

    #[test]
    fn tuning_sets_the_settle_time_only_in_min_hiss() {
        let dev = FakeDevice::in_mode(Mode::MinHiss);
        assert!(tune_min_hiss(&mut dev.clone(), 300).unwrap());
        assert_eq!(dev.writes(), vec![w(0x1F15, "1e")]);
        assert_eq!(state(&dev).settle_ms(), Some(300));
        assert_eq!(state(&dev).mode(), Some(Mode::MinHiss));
        assert!(!tune_min_hiss(&mut dev.clone(), 300).unwrap());
        assert_eq!(dev.writes().len(), 1);
        assert_eq!(dev.streams_opened(), 0);

        for bad in [0, 10, 605, 2010] {
            assert!(matches!(
                tune_min_hiss(&mut dev.clone(), bad),
                Err(Error::Refused(_))
            ));
        }
        for mode in [Mode::Stock, Mode::Quiet, Mode::Balanced] {
            let dev = FakeDevice::in_mode(mode);
            assert!(matches!(
                tune_min_hiss(&mut dev.clone(), 300),
                Err(Error::Refused(_))
            ));
            assert!(dev.writes().is_empty());
        }
    }

    #[test]
    fn min_hiss_keeps_its_settle_time_when_applied_again() {
        let dev = FakeDevice::in_mode(Mode::MinHiss);
        tune_min_hiss(&mut dev.clone(), 300).unwrap();
        dev.clear_writes();
        assert_eq!(apply(&dev, Mode::MinHiss).unwrap(), 0);
        assert!(dev.writes().is_empty());
        assert_eq!(state(&dev).settle_ms(), Some(300));
    }

    #[test]
    fn leaving_a_tuned_min_hiss_puts_the_settle_time_back_first() {
        for to in [Mode::Stock, Mode::Quiet, Mode::Balanced] {
            let dev = FakeDevice::in_mode(Mode::MinHiss);
            tune_min_hiss(&mut dev.clone(), 300).unwrap();
            dev.clear_writes();
            apply(&dev, to).unwrap();
            assert_eq!(dev.writes()[0], w(0x1F15, "3c"), "{to:?}");
            assert_eq!(dev.mem(), FakeDevice::in_mode(to).mem(), "{to:?}");
        }
    }

    #[test]
    fn mic_boost_sets_value_and_rejects_out_of_range() {
        let dev = FakeDevice::stock();
        assert!(set_mic_boost(&mut dev.clone(), 6).unwrap());
        assert_eq!(dev.byte(0x1017), 0x3E);
        assert!(matches!(
            set_mic_boost(&mut dev.clone(), 8),
            Err(Error::Refused(_))
        ));
        assert_eq!(dev.writes(), vec![w(0x1017, "3e")]);
        assert_eq!(dev.streams_opened(), 0);
    }

    #[test]
    fn mic_boost_survives_mode_changes() {
        let dev = FakeDevice::stock();
        set_mic_boost(&mut dev.clone(), 6).unwrap();
        for to in Mode::ALL {
            apply(&dev, to).unwrap();
            assert_eq!(dev.byte(0x1017), 0x3E);
        }
    }

    #[test]
    fn mode_names_parse_back() {
        for mode in Mode::ALL {
            assert_eq!(Mode::parse(mode.name()), Some(mode));
        }
        assert_eq!(Mode::parse("clamp"), None);
        assert_eq!(Mode::parse("idle"), None);
    }
}
