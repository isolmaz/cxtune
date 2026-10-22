//! Output modes, device state readout, and planning the patch sequence that moves the
//! device from its current state to a mode.

use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::patches::{
    self, DAC_ENABLE, DAC_HOLD_OFF, DAC_HOLD_ON, DAC_PREFIX, DAC_SUFFIX, HP_DRIVER, JMP, MIC_MAX,
    MIC_MIN, Outcome, Part, Patch, ROM_ID, ROM_ID_ADDR, RTS, SETTLE_DEFAULT, Tunable,
    VENDOR_OPERANDS, WINDOW_OPERANDS, Window, apply_patch,
};
use crate::protocol::{Transport, read_mem};
use crate::{Error, Result, hex};

/// A running playback stream of digital silence on the dongle's output. Dropping it ends the
/// stream.
pub trait Stream {
    /// Fails if the stream has stopped since it was opened.
    fn check(&self) -> Result<()>;
}

/// How long a freshly opened playback stream may take to bring the driver fully up.
pub const READY_TIMEOUT: Duration = Duration::from_secs(15);
const READY_POLL: Duration = Duration::from_millis(50);

/// The settle time is set in steps of this many milliseconds.
pub const SETTLE_STEP_MS: u16 = 10;

/// An output mode. The user-facing names are Default, Low click, Balanced and Lowest noise.
/// A mode is also the base of a custom setup: the balanced and lowest-noise routines each have a
/// time that can be changed in place ([`tune_decay`], [`tune_min_hiss`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Default: vendor behaviour. DACs and headphone driver always powered. Constant noise
    /// floor, no clicks.
    Stock,
    /// Low click: ROM default idle power-down. The driver parks at 0x33 when idle. Low clicks,
    /// some idle noise floor.
    Quiet,
    /// Balanced: driver clamped at 0x77 and analog references off when idle; ROM power-up order
    /// on start.
    Balanced,
    /// Lowest noise: driver clamped at 0x77 when idle; on start it first holds 0x33 for the
    /// soft-start time. Loud click at the start.
    MinHiss,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Stock, Mode::Quiet, Mode::Balanced, Mode::MinHiss];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Stock => "default",
            Mode::Quiet => "low-click",
            Mode::Balanced => "balanced",
            Mode::MinHiss => "lowest-noise",
        }
    }

    /// The mode called `s`. `stock`, `quiet`, `click-free` and `min-hiss` are accepted as other
    /// names of Default, Low click and Lowest noise.
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "stock" => Some(Mode::Stock),
            "quiet" | "click-free" => Some(Mode::Quiet),
            "min-hiss" => Some(Mode::MinHiss),
            _ => Mode::ALL.into_iter().find(|m| m.name() == s),
        }
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
    /// $1001..$1005: firmware version and chip id.
    pub rom: [u8; ROM_ID.len()],
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
    /// $1151 DSP enable bits.
    pub dsp_enable: u8,
    /// $1276 sidetone gain.
    pub sidetone_level: u16,
    /// $1152 second DSP enable byte: bit 3 is the virtual surround.
    pub surround: u8,
    /// $10D3 virtual surround level (low nibble) and variant (bit 4).
    pub surround_level: u8,
    /// $00A4 applier flags (read-only).
    pub appliers: u8,
    /// $00A8 mixer flags (read-only).
    pub mixer: u8,
    /// $13DC capture interface alternate setting (read-only).
    pub mic_stream: u8,
}

pub fn read_state<T: Transport + ?Sized>(t: &mut T) -> Result<State> {
    let mut r = |addr: u16, len: usize| read_mem(t, addr, len);
    let rom = r(ROM_ID_ADDR, ROM_ID.len())?;
    let hook = r(patches::HOOK, 4)?;
    let dac_a = r(patches::DAC_A_ROUTINE, 10)?;
    let dac_b = r(patches::DAC_B_ROUTINE, 10)?;
    let window = r(patches::WINDOW, patches::WINDOW_LEN)?;
    let mic = r(patches::MIC_CONTROL, 1)?;
    let dac_enable = r(DAC_ENABLE, 1)?;
    let hp = r(HP_DRIVER, 2)?;
    let reg_0f2c = r(0x0F2C, 1)?;
    let refs = r(patches::REFS, 1)?;
    let dsp_enable = r(patches::DSP_ENABLE, 1)?;
    let sidetone_level = r(patches::SIDETONE_LEVEL_ADDR, 2)?;
    let surround = r(patches::SURROUND_ADDR, 1)?;
    let surround_level = r(patches::SURROUND_LEVEL_ADDR, 1)?;
    let appliers = r(patches::APPLIERS, 1)?;
    let mixer = r(patches::MIXER, 1)?;
    let mic_stream = r(patches::MIC_STREAM, 1)?;
    Ok(State {
        rom: rom.try_into().unwrap(),
        hook: hook.try_into().unwrap(),
        dac_routines: [dac_a.try_into().unwrap(), dac_b.try_into().unwrap()],
        window: window.try_into().unwrap(),
        mic_control: mic[0],
        dac_enable: dac_enable[0],
        hp_driver: hp[0],
        reg_0f1b: hp[1],
        reg_0f2c: reg_0f2c[0],
        refs: refs[0],
        dsp_enable: dsp_enable[0],
        sidetone_level: u16::from_le_bytes([sidetone_level[0], sidetone_level[1]]),
        surround: surround[0],
        surround_level: surround_level[0],
        appliers: appliers[0],
        mixer: mixer[0],
        mic_stream: mic_stream[0],
    })
}

fn part_matches(window: Window, part: Part, got: &[u8]) -> bool {
    let want = part.of(window.image());
    match window.tunable() {
        Some(t) if part == Part::First => {
            got[..t.index] == want[..t.index]
                && got[t.index + 1..] == want[t.index + 1..]
                && t.contains(got[t.index])
        }
        _ => got == want,
    }
}

impl State {
    /// What each part of the window holds, or `None` if either matches no known routine.
    /// A routine with a tunable time matches with any value of it in the allowed range, but a
    /// tuned one only as a whole routine: next to another routine's second part it is not a
    /// state the planner can leave, since putting the default back is guarded on the whole
    /// routine.
    pub fn parts(&self) -> Option<[Window; 2]> {
        let find = |part: Part| {
            let got = &self.window[part.offset()..part.offset() + part.size()];
            [Window::Vendor, Window::Balanced, Window::MinHiss]
                .into_iter()
                .find(|&w| part_matches(w, part, got))
        };
        let parts = [find(Part::First)?, find(Part::Second)?];
        let tuned = parts[0]
            .tunable()
            .is_some_and(|t| self.window[t.index] != t.default);
        (!tuned || parts[1] == parts[0]).then_some(parts)
    }

    /// The tunable time of `window` in 10 ms units, if that routine is in the window.
    fn tuned_units(&self, window: Window) -> Option<u8> {
        let t = window.tunable()?;
        (self.parts()?[0] == window).then(|| self.window[t.index])
    }

    /// The lowest-noise soft-start time in 10 ms units, if that routine is in the window.
    pub fn settle_units(&self) -> Option<u8> {
        self.tuned_units(Window::MinHiss)
    }

    pub fn settle_ms(&self) -> Option<u16> {
        self.settle_units().map(|u| u as u16 * SETTLE_STEP_MS)
    }

    /// The balanced decay time in 10 ms units, if that routine is in the window.
    pub fn decay_units(&self) -> Option<u8> {
        self.tuned_units(Window::Balanced)
    }

    pub fn decay_ms(&self) -> Option<u16> {
        self.decay_units().map(|u| u as u16 * SETTLE_STEP_MS)
    }

    /// True if virtual surround is switched on.
    pub fn surround_on(&self) -> bool {
        self.surround & patches::SURROUND_BIT != 0
    }

    /// The virtual surround level, or `None` if the byte holds a level the ROM would clamp or bits
    /// cxtune does not know.
    pub fn surround_level(&self) -> Option<u8> {
        let level = self.surround_level & 0x0F;
        let unknown = self.surround_level & 0xE0 != 0;
        (!unknown && (patches::SURROUND_LEVEL_MIN..=patches::SURROUND_LEVEL_MAX).contains(&level))
            .then_some(level)
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
        if self.rom != ROM_ID {
            return Err(Error::Refused(format!(
                "unknown firmware at ${ROM_ID_ADDR:04X}: {}, expected {}",
                hex(&self.rom),
                hex(&ROM_ID)
            )));
        }
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
/// A changed time of the routine in the window (the lowest-noise soft-start time or the balanced
/// decay time) is put back to the default before the routine is replaced.
pub fn plan(state: &State, target: Mode) -> Result<Vec<Patch>> {
    let from = state.layout()?;
    let to = target.layout();

    let operands_change = from.window_operands != to.window_operands;
    let parts_change = from.parts != to.parts;
    let leave = from.jmp && (operands_change || parts_change || !to.jmp);
    let enter = to.jmp && (leave || !from.jmp);

    let mut hook = Vec::new();
    if let Some(t) = from.parts[0].tunable()
        && state.window[t.index] != t.default
        && from.parts[0] != to.parts[0]
    {
        hook.push(patches::tune(from.parts[0], t.default)?);
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
/// analog references up before balanced mode is left. Nothing is written until the codec
/// shows the stream, and a write is refused once the stream has stopped or the codec no
/// longer shows playback.
pub fn apply_mode<T: Transport + ?Sized, S: Stream>(
    t: &mut T,
    target: Mode,
    open_stream: impl FnOnce() -> Result<S>,
) -> Result<usize> {
    run(t, target, open_stream, READY_TIMEOUT, READY_POLL)
}

fn run<T: Transport + ?Sized, S: Stream>(
    t: &mut T,
    target: Mode,
    open_stream: impl FnOnce() -> Result<S>,
    ready_timeout: Duration,
    ready_poll: Duration,
) -> Result<usize> {
    let steps = plan(&read_state(t)?, target)?;
    let stream = if steps.iter().any(Patch::needs_stream) {
        let stream = open_stream()?;
        wait_ready(t, &stream, ready_timeout, ready_poll)?;
        Some(stream)
    } else {
        None
    };
    let mut written = 0;
    for p in &steps {
        if let Some(stream) = &stream
            && p.needs_stream()
        {
            stream.check()?;
        }
        if apply_patch(t, p)? == Outcome::Written {
            written += 1;
        }
    }
    Ok(written)
}

/// Waits until the codec shows the stream: the DAC enabled, both drivers fully on and the
/// analog references up. In stock mode the vendor patch forces everything but `$0F1B`, so
/// that one tells playback from idle.
fn wait_ready<T: Transport + ?Sized, S: Stream>(
    t: &mut T,
    stream: &S,
    timeout: Duration,
    poll: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        stream.check()?;
        let enable = read_mem(t, DAC_ENABLE, 1)?[0];
        let drivers = read_mem(t, HP_DRIVER, 2)?;
        let refs = read_mem(t, patches::REFS, 1)?[0];
        if enable & 1 != 0 && drivers == [0x00, 0x00] && refs & patches::REFS_ON != 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Audio(format!(
                "the dongle did not show the silent stream within {} s ($0F18={enable:#04x} \
                 $0F1A={:#04x} $0F1B={:#04x} $0F3E={refs:#04x}); nothing was written. \
                 Check that no other program holds the output; if the references \
                 ($0F3E bit 1) stay off, replug the dongle",
                timeout.as_secs(),
                drivers[0],
                drivers[1]
            )));
        }
        sleep(poll);
    }
}

/// Converts a time in milliseconds to the 10 ms units of `t`.
fn time_units(t: Tunable, ms: u16) -> Result<u8> {
    let units = ms / SETTLE_STEP_MS;
    if !ms.is_multiple_of(SETTLE_STEP_MS) || !(t.min as u16..=t.max as u16).contains(&units) {
        return Err(Error::Refused(format!(
            "{} must be {}..={} ms in steps of {SETTLE_STEP_MS} ms, got {ms}",
            t.what,
            t.min as u16 * SETTLE_STEP_MS,
            t.max as u16 * SETTLE_STEP_MS
        )));
    }
    Ok(units as u8)
}

/// Converts a soft-start time in milliseconds to 10 ms units.
pub fn settle_units(ms: u16) -> Result<u8> {
    time_units(Window::MinHiss.tunable().expect("tunable"), ms)
}

/// Converts a decay time in milliseconds to 10 ms units.
pub fn decay_units(ms: u16) -> Result<u8> {
    time_units(Window::Balanced.tunable().expect("tunable"), ms)
}

/// Sets the lowest-noise soft-start time (milliseconds). Returns true if a byte was written.
pub fn tune_min_hiss<T: Transport + ?Sized>(t: &mut T, ms: u16) -> Result<bool> {
    let units = settle_units(ms)?;
    if read_state(t)?.settle_units().is_none() {
        return Err(Error::Refused(
            "the lowest-noise routine is not installed; apply the lowest-noise mode first".into(),
        ));
    }
    Ok(apply_patch(t, &patches::settle_time(units)?)? == Outcome::Written)
}

/// Sets the balanced stop decay time (milliseconds). Returns true if a byte was written.
pub fn tune_decay<T: Transport + ?Sized>(t: &mut T, ms: u16) -> Result<bool> {
    let units = decay_units(ms)?;
    if read_state(t)?.decay_units().is_none() {
        return Err(Error::Refused(
            "the balanced routine is not installed; apply the balanced mode first".into(),
        ));
    }
    Ok(apply_patch(t, &patches::decay_time(units)?)? == Outcome::Written)
}

/// The registers that tell whether the output is idle, read from the dongle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output {
    /// $0F18.
    pub dac_enable: u8,
    /// $0F1A.
    pub hp_driver: u8,
    /// $0F1B.
    pub reg_0f1b: u8,
    /// $0F3E.
    pub refs: u8,
    /// $13DC: the capture interface is in use.
    pub capture_stream: u8,
    /// $13DD: the playback interface is in use.
    pub playback_stream: u8,
    /// $00A5: bit 0 is set while the codec captures.
    pub capture_active: u8,
}

pub fn read_output<T: Transport + ?Sized>(t: &mut T) -> Result<Output> {
    let dac = read_mem(t, DAC_ENABLE, 1)?;
    let drivers = read_mem(t, HP_DRIVER, 2)?;
    let refs = read_mem(t, patches::REFS, 1)?;
    let streams = read_mem(t, patches::MIC_STREAM, 2)?;
    let capture = read_mem(t, patches::CAPTURE_ACTIVE, 1)?;
    Ok(Output {
        dac_enable: dac[0],
        hp_driver: drivers[0],
        reg_0f1b: drivers[1],
        refs: refs[0],
        capture_stream: streams[0],
        playback_stream: streams[1],
        capture_active: capture[0],
    })
}

impl Output {
    /// True when no stream is open on either interface and the output has settled into the idle
    /// state of `mode` (`None`: a mode that is not known): the DAC off, and the driver parked
    /// (low click) or clamped with the references off (balanced) or clamped (lowest noise). In
    /// the default mode the vendor patch holds the driver and the DAC on, so the second driver
    /// (`$0F1B` at 0x33, in every mode) alone tells.
    pub fn is_idle(&self, mode: Option<Mode>) -> bool {
        let no_streams =
            self.capture_stream == 0 && self.playback_stream == 0 && self.capture_active & 1 == 0;
        let parked = self.reg_0f1b == 0x33;
        let dac_off = self.dac_enable & 1 == 0;
        let refs_off = self.refs & patches::REFS_ON == 0;
        no_streams
            && parked
            && match mode {
                Some(Mode::Stock) => true,
                Some(Mode::Quiet) => dac_off && self.hp_driver == 0x33,
                Some(Mode::Balanced) => dac_off && self.hp_driver == 0x77 && refs_off,
                Some(Mode::MinHiss) => dac_off && self.hp_driver == 0x77,
                None => dac_off,
            }
    }
}

/// How long the first audio of a stream that starts from idle is muted, in ms (measured, see
/// docs/firmware-notes.md): nothing in the default and low-click modes, about 650 ms in
/// balanced, and the soft-start time plus about 150 ms in lowest noise. `soft_start_ms` is the
/// lowest-noise soft-start time (the routine's default if `None`); a mode that is not known is
/// taken as the longest of the fixed ones.
pub fn mute_delay_ms(mode: Option<Mode>, soft_start_ms: Option<u16>) -> u32 {
    match mode {
        Some(Mode::Stock | Mode::Quiet) => 0,
        Some(Mode::Balanced) | None => 650,
        Some(Mode::MinHiss) => {
            u32::from(soft_start_ms.unwrap_or(SETTLE_DEFAULT as u16 * SETTLE_STEP_MS)) + 150
        }
    }
}

/// Sets the analog mic boost step (0..=7, $1017 = 0x38 + step). Returns true if written.
pub fn set_mic_boost<T: Transport + ?Sized>(t: &mut T, step: u8) -> Result<bool> {
    Ok(apply_patch(t, &patches::mic_boost(step)?)? == Outcome::Written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{BALANCED_HEX, FakeDevice, MIN_HISS_HEX, VENDOR_HEX, bytes};

    const FAST_POLL: Duration = Duration::from_millis(1);

    fn apply(dev: &FakeDevice, target: Mode) -> Result<usize> {
        run(
            &mut dev.clone(),
            target,
            || dev.open_stream(),
            READY_TIMEOUT,
            FAST_POLL,
        )
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
            let decay = (mode == Mode::Balanced).then_some(800);
            assert_eq!(s.decay_ms(), decay, "{mode:?}");
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
                assert_eq!(dev.early_writes(), 0, "{from:?} -> {to:?}");
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
        let regions: [(u16, usize); 5] = [
            (0x1001, 5),
            (0x14B1, 4),
            (0x1EFC, 43),
            (0x16C9, 10),
            (0x17CA, 10),
        ];
        for from in Mode::ALL {
            for (addr, len) in regions {
                for a in addr..addr + len as u16 {
                    let dev = FakeDevice::in_mode(from);
                    // a time outside its range is the only change to its byte that the
                    // routine does not accept
                    let bad = if a == 0x1F15 || a == 0x1F0E {
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
        assert!(matches!(apply(&dev, Mode::Balanced), Err(Error::Audio(_))));
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
                FAST_POLL,
            )
            .unwrap_err();
            assert!(matches!(err, Error::Audio(_)), "{to:?}: {err}");
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
            FAST_POLL,
        )
        .unwrap_err();
        let Error::Audio(message) = err else {
            panic!("{err}")
        };
        assert!(message.contains("$0F3E=0x00"), "{message}");
    }

    #[test]
    fn the_wait_ends_only_when_the_codec_shows_the_stream() {
        // stock forces the DACs, the driver and the references on, so only $0F1B differs
        let dev = FakeDevice::in_mode(Mode::Stock);
        let stream = dev.open_stream().unwrap();
        assert!(!dev.codec_sees_stream());
        wait_ready(&mut dev.clone(), &stream, READY_TIMEOUT, FAST_POLL).unwrap();
        assert!(dev.codec_sees_stream());
    }

    #[test]
    fn nothing_is_written_when_the_codec_never_shows_the_stream() {
        for from in Mode::ALL {
            let dev = FakeDevice::in_mode(from);
            dev.never_show_streams();
            let err = run(
                &mut dev.clone(),
                Mode::Balanced,
                || dev.open_stream(),
                Duration::from_millis(40),
                FAST_POLL,
            );
            if from == Mode::Balanced {
                assert_eq!(err.unwrap(), 0);
                continue;
            }
            let Err(Error::Audio(message)) = err else {
                panic!("{from:?}: {err:?}")
            };
            assert!(message.contains("$0F1B="), "{from:?}: {message}");
            assert!(dev.writes().is_empty(), "{from:?}");
            assert!(!dev.stream_is_open());
        }
    }

    #[test]
    fn a_stream_that_stops_ends_the_change_before_the_next_stream_write() {
        for from in Mode::ALL {
            for to in Mode::ALL {
                let steps = plan(&state(&FakeDevice::in_mode(from)), to).unwrap();
                for done in 1..steps.len() {
                    let label = format!("{from:?} -> {to:?}, stream stops after {done}");
                    let dev = FakeDevice::in_mode(from);
                    dev.stop_stream_after_writes(done);
                    let result = apply(&dev, to);
                    if let Some(skipped) = steps[done..].iter().position(Patch::needs_stream) {
                        let err = result.expect_err(&label);
                        assert!(
                            err.to_string().contains("silent stream stopped"),
                            "{label}: {err}"
                        );
                        // DAC holds before the next stream patch need no stream
                        assert_eq!(dev.writes().len(), done + skipped, "{label}");
                    } else {
                        // only DAC holds are left
                        result.unwrap_or_else(|e| panic!("{label}: {e}"));
                    }
                    apply(&dev, to).unwrap_or_else(|e| panic!("{label}: {e}"));
                    assert_eq!(dev.mem(), FakeDevice::in_mode(to).mem(), "{label}");
                }
            }
        }
    }

    #[test]
    fn a_stream_that_is_already_dead_ends_the_wait() {
        struct Dead;
        impl Stream for Dead {
            fn check(&self) -> Result<()> {
                Err(Error::Audio("dead stream".into()))
            }
        }
        let dev = FakeDevice::in_mode(Mode::Stock);
        let err = run(
            &mut dev.clone(),
            Mode::Balanced,
            || Ok(Dead),
            Duration::from_millis(300),
            FAST_POLL,
        )
        .unwrap_err();
        assert!(err.to_string().contains("dead stream"), "{err}");
        assert!(dev.writes().is_empty());
    }

    #[test]
    fn a_tuned_settle_time_next_to_another_routine_is_not_a_state() {
        let dev = FakeDevice::in_mode(Mode::MinHiss);
        dev.poke(0x14B1, &[0x60]);
        dev.poke(0x1F1C, &bytes(VENDOR_HEX)[32..]);
        dev.settle();
        // the default settle time leaves a state the planner can move on from
        assert_eq!(state(&dev).parts(), Some([Window::MinHiss, Window::Vendor]));
        assert!(plan(&state(&dev), Mode::Quiet).is_ok());

        dev.poke(0x1F15, &[0x1E]);
        assert_eq!(state(&dev).parts(), None);
        for to in Mode::ALL {
            assert!(matches!(apply(&dev, to), Err(Error::Refused(_))), "{to:?}");
        }
        assert!(dev.writes().is_empty());
        assert_eq!(dev.streams_opened(), 0);
    }

    #[test]
    fn a_tuned_decay_time_next_to_another_routine_is_not_a_state() {
        let dev = FakeDevice::in_mode(Mode::Balanced);
        dev.poke(0x14B1, &[0x60]);
        dev.poke(0x1F1C, &bytes(VENDOR_HEX)[32..]);
        dev.settle();
        assert_eq!(
            state(&dev).parts(),
            Some([Window::Balanced, Window::Vendor])
        );
        assert!(plan(&state(&dev), Mode::Quiet).is_ok());

        dev.poke(0x1F0E, &[0x1E]);
        assert_eq!(state(&dev).parts(), None);
        for to in Mode::ALL {
            assert!(matches!(apply(&dev, to), Err(Error::Refused(_))), "{to:?}");
        }
        assert!(dev.writes().is_empty());
        assert_eq!(dev.streams_opened(), 0);
    }

    #[test]
    fn a_decay_time_outside_its_range_is_not_a_state() {
        for bad in [0x00, 0x09, 0xC9, 0xFF] {
            let dev = FakeDevice::in_mode(Mode::Balanced);
            dev.poke(0x1F0E, &[bad]);
            assert_eq!(state(&dev).mode(), None, "{bad:#04x}");
            assert_eq!(state(&dev).decay_ms(), None, "{bad:#04x}");
            assert!(matches!(
                apply(&dev, Mode::Balanced),
                Err(Error::Refused(_))
            ));
        }
        for ok in [0x0A, 0x50, 0xC8] {
            let dev = FakeDevice::in_mode(Mode::Balanced);
            dev.poke(0x1F0E, &[ok]);
            assert_eq!(state(&dev).mode(), Some(Mode::Balanced), "{ok:#04x}");
            assert_eq!(state(&dev).decay_ms(), Some(u16::from(ok) * 10));
        }
    }

    #[test]
    fn decay_time_converts_from_milliseconds() {
        assert_eq!(decay_units(100).unwrap(), 0x0A);
        assert_eq!(decay_units(800).unwrap(), 0x50);
        assert_eq!(decay_units(2000).unwrap(), 0xC8);
        for bad in [0, 10, 90, 95, 105, 805, 2010, 2500, u16::MAX] {
            assert!(matches!(decay_units(bad), Err(Error::Refused(_))), "{bad}");
        }
    }

    #[test]
    fn tuning_sets_the_decay_time_only_in_balanced() {
        let dev = FakeDevice::in_mode(Mode::Balanced);
        assert!(tune_decay(&mut dev.clone(), 300).unwrap());
        assert_eq!(dev.writes(), vec![w(0x1F0E, "1e")]);
        assert_eq!(state(&dev).decay_ms(), Some(300));
        assert_eq!(state(&dev).mode(), Some(Mode::Balanced));
        assert!(!tune_decay(&mut dev.clone(), 300).unwrap());
        assert_eq!(dev.writes().len(), 1);
        assert_eq!(dev.streams_opened(), 0);

        for bad in [0, 90, 805, 2010] {
            assert!(matches!(
                tune_decay(&mut dev.clone(), bad),
                Err(Error::Refused(_))
            ));
        }
        for mode in [Mode::Stock, Mode::Quiet, Mode::MinHiss] {
            let dev = FakeDevice::in_mode(mode);
            assert!(matches!(
                tune_decay(&mut dev.clone(), 300),
                Err(Error::Refused(_))
            ));
            assert!(dev.writes().is_empty());
        }
    }

    #[test]
    fn the_two_times_are_tuned_in_their_own_routines_only() {
        let dev = FakeDevice::in_mode(Mode::Balanced);
        assert!(matches!(
            tune_min_hiss(&mut dev.clone(), 300),
            Err(Error::Refused(_))
        ));
        let dev = FakeDevice::in_mode(Mode::MinHiss);
        assert!(matches!(
            tune_decay(&mut dev.clone(), 300),
            Err(Error::Refused(_))
        ));
        assert_eq!(state(&dev).decay_ms(), None);
        assert_eq!(state(&dev).settle_ms(), Some(600));
    }

    #[test]
    fn balanced_keeps_its_decay_time_when_applied_again() {
        let dev = FakeDevice::in_mode(Mode::Balanced);
        tune_decay(&mut dev.clone(), 300).unwrap();
        dev.clear_writes();
        assert_eq!(apply(&dev, Mode::Balanced).unwrap(), 0);
        assert!(dev.writes().is_empty());
        assert_eq!(state(&dev).decay_ms(), Some(300));
    }

    #[test]
    fn leaving_a_tuned_balanced_puts_the_decay_time_back_first() {
        for to in [Mode::Stock, Mode::Quiet, Mode::MinHiss] {
            let dev = FakeDevice::in_mode(Mode::Balanced);
            tune_decay(&mut dev.clone(), 300).unwrap();
            dev.clear_writes();
            apply(&dev, to).unwrap();
            assert_eq!(dev.writes()[0], w(0x1F0E, "50"), "{to:?}");
            assert_eq!(dev.mem(), FakeDevice::in_mode(to).mem(), "{to:?}");
        }
    }

    #[test]
    fn a_change_that_stopped_after_the_decay_time_went_back_can_be_completed() {
        for to in [Mode::Stock, Mode::Quiet, Mode::MinHiss] {
            for stop_after in 1..=3 {
                let dev = FakeDevice::in_mode(Mode::Balanced);
                tune_decay(&mut dev.clone(), 300).unwrap();
                dev.clear_writes();
                dev.fail_after_writes(stop_after);
                assert!(apply(&dev, to).is_err(), "{to:?} after {stop_after}");
                dev.clear_failure();
                dev.clear_writes();
                apply(&dev, to).unwrap_or_else(|e| panic!("{to:?} after {stop_after}: {e}"));
                assert_eq!(dev.mem(), FakeDevice::in_mode(to).mem(), "{to:?}");
            }
        }
    }

    #[test]
    fn the_surround_is_read_from_its_two_bytes() {
        let dev = FakeDevice::stock();
        assert!(!state(&dev).surround_on());
        assert_eq!(state(&dev).surround_level(), Some(4));
        dev.poke(0x1152, &[0x08]);
        dev.poke(0x10D3, &[0x0A]);
        assert!(state(&dev).surround_on());
        assert_eq!(state(&dev).surround_level(), Some(10));
        // the preset bits do not matter, other bits and levels out of range are not ours
        dev.poke(0x1152, &[0x0A]);
        assert!(state(&dev).surround_on());
        for bad in [0x00, 0x0B, 0x24, 0x84] {
            dev.poke(0x10D3, &[bad]);
            assert_eq!(state(&dev).surround_level(), None, "{bad:#04x}");
        }
        // the variant bit is not part of the level
        dev.poke(0x10D3, &[0x15]);
        assert_eq!(state(&dev).surround_level(), Some(5));
        // neither setting is part of a mode
        for mode in Mode::ALL {
            let dev = FakeDevice::in_mode(mode);
            dev.poke(0x1152, &[0x08]);
            dev.poke(0x10D3, &[0x0A]);
            assert_eq!(state(&dev).mode(), Some(mode));
        }
    }

    fn output(dev: &FakeDevice) -> Output {
        read_output(&mut dev.clone()).unwrap()
    }

    #[test]
    fn an_idle_output_is_idle_in_its_own_mode_only() {
        // (mode of the device, mode asked about, idle)
        for (device, asked, idle) in [
            (Mode::Stock, Some(Mode::Stock), true),
            (Mode::Stock, Some(Mode::Quiet), false),
            (Mode::Stock, Some(Mode::Balanced), false),
            (Mode::Stock, Some(Mode::MinHiss), false),
            (Mode::Stock, None, false),
            (Mode::Quiet, Some(Mode::Quiet), true),
            (Mode::Quiet, Some(Mode::Balanced), false),
            (Mode::Quiet, Some(Mode::MinHiss), false),
            (Mode::Quiet, None, true),
            (Mode::Balanced, Some(Mode::Balanced), true),
            (Mode::Balanced, Some(Mode::Quiet), false),
            (Mode::Balanced, None, true),
            (Mode::MinHiss, Some(Mode::MinHiss), true),
            (Mode::MinHiss, Some(Mode::Balanced), false),
            (Mode::MinHiss, Some(Mode::Quiet), false),
            (Mode::MinHiss, None, true),
        ] {
            let dev = FakeDevice::in_mode(device);
            assert_eq!(
                output(&dev).is_idle(asked),
                idle,
                "{device:?} asked as {asked:?}"
            );
        }
    }

    #[test]
    fn the_output_is_not_idle_while_a_stream_is_open_whatever_the_mode() {
        for mode in Mode::ALL {
            let dev = FakeDevice::in_mode(mode);
            let mic = dev.open_mic();
            assert!(!output(&dev).is_idle(Some(mode)), "{mode:?} capturing");
            assert!(!output(&dev).is_idle(None), "{mode:?} capturing");
            drop(mic);

            let stream = dev.open_active_stream().unwrap();
            assert!(!output(&dev).is_idle(Some(mode)), "{mode:?} playing");
            assert!(!output(&dev).is_idle(None), "{mode:?} playing");
            drop(stream);
        }
        // registers that look idle but a stream is still open
        let dev = FakeDevice::in_mode(Mode::Quiet);
        dev.poke(0x13DD, &[1]);
        assert!(!output(&dev).is_idle(Some(Mode::Quiet)));
        dev.poke(0x13DD, &[0]);
        dev.poke(0x13DC, &[1]);
        assert!(!output(&dev).is_idle(Some(Mode::Quiet)));
        dev.poke(0x13DC, &[0]);
        dev.poke(0x00A5, &[1]);
        assert!(!output(&dev).is_idle(Some(Mode::Quiet)));
        dev.poke(0x00A5, &[0]);
        assert!(output(&dev).is_idle(Some(Mode::Quiet)));
    }

    #[test]
    fn balanced_is_not_idle_with_the_references_up_or_the_driver_open() {
        let dev = FakeDevice::in_mode(Mode::Balanced);
        assert!(output(&dev).is_idle(Some(Mode::Balanced)));
        dev.poke(0x0F3E, &[0x02]);
        assert!(!output(&dev).is_idle(Some(Mode::Balanced)));
        dev.poke(0x0F3E, &[0x00]);
        dev.poke(0x0F1A, &[0x00]);
        assert!(!output(&dev).is_idle(Some(Mode::Balanced)));
        dev.poke(0x0F1A, &[0x77]);
        dev.poke(0x0F18, &[0x03]);
        assert!(!output(&dev).is_idle(Some(Mode::Balanced)));
        dev.poke(0x0F18, &[0x00]);
        dev.poke(0x0F1B, &[0x00]);
        assert!(!output(&dev).is_idle(Some(Mode::Balanced)));
        dev.poke(0x0F1B, &[0x33]);
        assert!(output(&dev).is_idle(Some(Mode::Balanced)));
    }

    #[test]
    fn balanced_opens_the_driver_for_a_capture_and_clamps_it_after_the_decay() {
        let dev = FakeDevice::in_mode(Mode::Balanced);
        dev.hold_driver_after_capture(40);
        let mic = dev.open_mic();
        let o = output(&dev);
        assert_eq!(o.refs & 0x02, 0x02, "up while the codec captures");
        assert_eq!(o.hp_driver, 0x33, "the driver is open for the capture");
        assert!(!o.is_idle(Some(Mode::Balanced)));
        drop(mic);
        let o = output(&dev);
        assert_eq!(o.capture_stream, 0);
        assert_eq!(o.capture_active & 1, 0);
        assert_eq!(o.refs & 0x02, 0, "the references drop at once");
        assert!(!o.is_idle(Some(Mode::Balanced)), "driver still open");
        let mut reads = 0;
        while !output(&dev).is_idle(Some(Mode::Balanced)) {
            reads += 1;
            assert!(reads < 50, "the driver never clamped");
        }
        assert!(reads > 1);

        // the other routines keep nothing open for a capture
        for mode in [Mode::Quiet, Mode::MinHiss] {
            let dev = FakeDevice::in_mode(mode);
            let mic = dev.open_mic();
            drop(mic);
            assert!(output(&dev).is_idle(Some(mode)), "{mode:?}");
        }
    }

    #[test]
    fn the_mute_delay_of_the_first_audio_follows_the_mode() {
        assert_eq!(mute_delay_ms(Some(Mode::Stock), None), 0);
        assert_eq!(mute_delay_ms(Some(Mode::Quiet), None), 0);
        assert_eq!(mute_delay_ms(Some(Mode::Balanced), None), 650);
        // a mode that is not known is taken as the longest of the fixed ones
        assert_eq!(mute_delay_ms(None, None), 650);
        // lowest noise: the soft-start time plus 150 ms, 600 ms by default
        assert_eq!(mute_delay_ms(Some(Mode::MinHiss), None), 750);
        assert_eq!(mute_delay_ms(Some(Mode::MinHiss), Some(600)), 750);
        assert_eq!(mute_delay_ms(Some(Mode::MinHiss), Some(20)), 170);
        assert_eq!(mute_delay_ms(Some(Mode::MinHiss), Some(2000)), 2150);
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
        assert_eq!(Mode::parse(""), None);
        assert_eq!(Mode::parse("Balanced"), None);
    }

    #[test]
    fn the_user_facing_names_are_the_mode_names() {
        let names: Vec<_> = Mode::ALL.iter().map(|m| m.name()).collect();
        assert_eq!(names, ["default", "low-click", "balanced", "lowest-noise"]);
    }

    #[test]
    fn the_other_names_of_the_modes_parse_to_them() {
        for (name, mode) in [
            ("stock", Mode::Stock),
            ("quiet", Mode::Quiet),
            ("click-free", Mode::Quiet),
            ("min-hiss", Mode::MinHiss),
        ] {
            assert_eq!(Mode::parse(name), Some(mode), "{name}");
        }
    }
}
