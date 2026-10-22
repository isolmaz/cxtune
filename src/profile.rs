//! Profiles, device state readout, and planning the patch sequence that moves the
//! device from its current state to a profile.

use crate::patches::{
    self, BODY_CLAMP, BODY_ORIG, CLAMP_OPERANDS, DAC_PREFIX, DAC_SUFFIX, HOOK_BACKWARD,
    HOOK_FORWARD, JMP, MIC_BOOST, MIC_MAX, MIC_MIN, Outcome, Patch, RTS, VENDOR_OPERANDS,
    apply_patch,
};
use crate::protocol::{Transport, read_mem};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Vendor behaviour: DACs and headphone driver always powered.
    Stock,
    /// ROM default idle power-down: $16CF=$17D0=0x00, $14B1=RTS.
    IdlePowerDown,
    /// Idle power-down plus the $1EFC routine that clamps the driver at 0x77 (experimental).
    DeepIdleClamp,
}

impl Profile {
    pub const ALL: [Profile; 3] = [
        Profile::Stock,
        Profile::IdlePowerDown,
        Profile::DeepIdleClamp,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Profile::Stock => "stock",
            Profile::IdlePowerDown => "idle",
            Profile::DeepIdleClamp => "clamp",
        }
    }

    pub fn parse(s: &str) -> Option<Profile> {
        Profile::ALL.into_iter().find(|p| p.name() == s)
    }

    fn hook_state(self) -> usize {
        match self {
            Profile::Stock => 0,
            Profile::IdlePowerDown => 1,
            Profile::DeepIdleClamp => 4,
        }
    }

    fn dac_hold(self) -> u8 {
        match self {
            Profile::Stock => patches::DAC_HOLD_ON,
            _ => patches::DAC_HOLD_OFF,
        }
    }
}

/// Raw bytes of every location the app knows about.
#[derive(Clone, Debug)]
pub struct State {
    /// $14B1..$14B4: hook30 opcode and operands.
    pub hook: [u8; 4],
    /// $16C9..$16D2 and $17CA..$17D3: the DAC-hold routines.
    pub dac_routines: [[u8; 10]; 2],
    /// $1EFC..$1F12: vendor bytes or the clamp routine.
    pub body: [u8; 23],
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
}

pub fn read_state<T: Transport + ?Sized>(t: &mut T) -> Result<State> {
    let mut r = |addr: u16, len: usize| read_mem(t, addr, len);
    let hook = r(patches::HOOK, 4)?;
    let dac_a = r(patches::DAC_A_ROUTINE, 10)?;
    let dac_b = r(patches::DAC_B_ROUTINE, 10)?;
    let body = r(patches::CLAMP_BODY, 23)?;
    let mic = r(patches::MIC_CONTROL, 1)?;
    let dac_enable = r(0x0F18, 1)?;
    let hp = r(0x0F1A, 2)?;
    let reg_0f2c = r(0x0F2C, 1)?;
    Ok(State {
        hook: hook.try_into().unwrap(),
        dac_routines: [dac_a.try_into().unwrap(), dac_b.try_into().unwrap()],
        body: body.try_into().unwrap(),
        mic_control: mic[0],
        dac_enable: dac_enable[0],
        hp_driver: hp[0],
        reg_0f1b: hp[1],
        reg_0f2c: reg_0f2c[0],
    })
}

impl State {
    /// Position in the hook state chain (see [`patches::HOOK_FORWARD`]), or `None` if
    /// the bytes match no known state.
    pub fn hook_state(&self) -> Option<usize> {
        let body_clamp = self.body == BODY_CLAMP;
        let body_orig = self.body == BODY_ORIG;
        let [op, lo, hi, tail] = self.hook;
        if tail != 0x4C {
            return None;
        }
        let vendor = [lo, hi] == VENDOR_OPERANDS;
        let clamp = [lo, hi] == CLAMP_OPERANDS;
        match (op, vendor, clamp, body_orig, body_clamp) {
            (JMP, true, _, true, _) => Some(0),
            (RTS, true, _, true, _) => Some(1),
            (RTS, true, _, _, true) => Some(2),
            (RTS, _, true, _, true) => Some(3),
            (JMP, _, true, _, true) => Some(4),
            _ => None,
        }
    }

    /// The DAC-hold immediate of each routine, or `None` if a routine does not match.
    pub fn dac_hold(&self) -> [Option<u8>; 2] {
        self.dac_routines.map(|r| {
            let ok = r[..6] == DAC_PREFIX
                && r[7..] == DAC_SUFFIX
                && (r[6] == patches::DAC_HOLD_ON || r[6] == patches::DAC_HOLD_OFF);
            ok.then_some(r[6])
        })
    }

    pub fn profile(&self) -> Option<Profile> {
        let hook = self.hook_state()?;
        let dac = self.dac_hold();
        Profile::ALL
            .into_iter()
            .find(|p| p.hook_state() == hook && dac == [Some(p.dac_hold()); 2])
    }

    /// Mic boost step 0..=7, or `None` if $1017 is outside 0x38..=0x3F.
    pub fn mic_boost(&self) -> Option<u8> {
        (MIC_MIN..=MIC_MAX)
            .contains(&self.mic_control)
            .then(|| self.mic_control - MIC_MIN)
    }
}

/// The ordered patches that move `state` to `target`. Fails without writing anything
/// if the current state is not recognised. When idling the DACs, they are released
/// before the hook changes; when restoring stock, the hook is restored first.
pub fn plan(state: &State, target: Profile) -> Result<Vec<&'static Patch>> {
    let from = state.hook_state().ok_or_else(|| {
        Error::Refused(format!(
            "unrecognised hook state ($14B1 {}, $1EFC {})",
            crate::hex(&state.hook),
            crate::hex(&state.body[..4])
        ))
    })?;
    let to = target.hook_state();
    let hook: Vec<&'static Patch> = if from <= to {
        HOOK_FORWARD[from..to].to_vec()
    } else {
        HOOK_BACKWARD[to..from].iter().rev().copied().collect()
    };

    let want = target.dac_hold();
    let mut dac = Vec::new();
    for (i, current) in state.dac_hold().into_iter().enumerate() {
        let current = current.ok_or_else(|| {
            Error::Refused(format!(
                "unrecognised DAC-hold routine at ${:04X}: {}",
                [patches::DAC_A_ROUTINE, patches::DAC_B_ROUTINE][i],
                crate::hex(&state.dac_routines[i])
            ))
        })?;
        if current != want {
            dac.push(match (i, want) {
                (0, patches::DAC_HOLD_OFF) => &patches::DAC_A_RELEASE,
                (0, _) => &patches::DAC_A_HOLD,
                (_, patches::DAC_HOLD_OFF) => &patches::DAC_B_RELEASE,
                _ => &patches::DAC_B_HOLD,
            });
        }
    }

    Ok(if want == patches::DAC_HOLD_OFF {
        dac.into_iter().chain(hook).collect()
    } else {
        hook.into_iter().chain(dac).collect()
    })
}

/// Moves the device to `target`. Returns the number of bytes-changing writes made.
pub fn apply_profile<T: Transport + ?Sized>(t: &mut T, target: Profile) -> Result<usize> {
    let state = read_state(t)?;
    let mut written = 0;
    for p in plan(&state, target)? {
        if apply_patch(t, p)? == Outcome::Written {
            written += 1;
        }
    }
    Ok(written)
}

/// Sets the analog mic boost step (0..=7, $1017 = 0x38 + step). Returns true if written.
pub fn set_mic_boost<T: Transport + ?Sized>(t: &mut T, step: u8) -> Result<bool> {
    let p = MIC_BOOST
        .get(step as usize)
        .ok_or_else(|| Error::Refused(format!("mic boost {step} is outside 0..=7")))?;
    Ok(apply_patch(t, p)? == Outcome::Written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeDevice, bytes};

    fn w(addr: u16, hex: &str) -> (u16, Vec<u8>) {
        (addr, bytes(hex))
    }

    const CLAMP_HEX: &str = "ad180f2903d00f6868a9778d1a0fad1b0f09338d1b0f60";
    const ORIG_HEX: &str = "2fbf1bcd8512f0168d85128dea13a9018d84128de913a9";

    #[test]
    fn stock_device_reads_as_stock() {
        let mut dev = FakeDevice::stock();
        let s = read_state(&mut dev).unwrap();
        assert_eq!(s.profile(), Some(Profile::Stock));
        assert_eq!(s.mic_boost(), Some(4));
        assert!(dev.writes.is_empty());
    }

    #[test]
    fn stock_to_idle() {
        let mut dev = FakeDevice::stock();
        assert_eq!(apply_profile(&mut dev, Profile::IdlePowerDown).unwrap(), 3);
        assert_eq!(
            dev.writes,
            vec![w(0x16CF, "00"), w(0x17D0, "00"), w(0x14B1, "60")]
        );
        assert_eq!(
            read_state(&mut dev).unwrap().profile(),
            Some(Profile::IdlePowerDown)
        );
    }

    #[test]
    fn stock_to_clamp_installs_body_then_operands_then_opcode() {
        let mut dev = FakeDevice::stock();
        assert_eq!(apply_profile(&mut dev, Profile::DeepIdleClamp).unwrap(), 6);
        assert_eq!(
            dev.writes,
            vec![
                w(0x16CF, "00"),
                w(0x17D0, "00"),
                w(0x14B1, "60"),
                w(0x1EFC, CLAMP_HEX),
                w(0x14B2, "fc1e"),
                w(0x14B1, "4c"),
            ]
        );
        assert_eq!(&dev.mem[0x14B1..0x14B5], bytes("4cfc1e4c").as_slice());
        assert_eq!(
            read_state(&mut dev).unwrap().profile(),
            Some(Profile::DeepIdleClamp)
        );
    }

    #[test]
    fn clamp_to_stock_unwinds_in_reverse_and_restores_memory() {
        let pristine = FakeDevice::stock().mem;
        let mut dev = FakeDevice::stock();
        apply_profile(&mut dev, Profile::DeepIdleClamp).unwrap();
        dev.writes.clear();
        assert_eq!(apply_profile(&mut dev, Profile::Stock).unwrap(), 6);
        assert_eq!(
            dev.writes,
            vec![
                w(0x14B1, "60"),
                w(0x14B2, "1b1f"),
                w(0x1EFC, ORIG_HEX),
                w(0x14B1, "4c"),
                w(0x16CF, "03"),
                w(0x17D0, "03"),
            ]
        );
        assert!(dev.mem == pristine);
    }

    #[test]
    fn clamp_to_idle_removes_routine() {
        let mut dev = FakeDevice::stock();
        apply_profile(&mut dev, Profile::DeepIdleClamp).unwrap();
        dev.writes.clear();
        apply_profile(&mut dev, Profile::IdlePowerDown).unwrap();
        assert_eq!(
            dev.writes,
            vec![w(0x14B1, "60"), w(0x14B2, "1b1f"), w(0x1EFC, ORIG_HEX)]
        );
    }

    #[test]
    fn resumes_from_interrupted_clamp_install() {
        let mut dev = FakeDevice::stock();
        dev.mem[0x16CF] = 0x00;
        dev.mem[0x17D0] = 0x00;
        dev.mem[0x14B1] = 0x60;
        dev.mem[0x1EFC..0x1EFC + 23].copy_from_slice(&bytes(CLAMP_HEX));
        apply_profile(&mut dev, Profile::DeepIdleClamp).unwrap();
        assert_eq!(dev.writes, vec![w(0x14B2, "fc1e"), w(0x14B1, "4c")]);
    }

    #[test]
    fn applying_twice_writes_nothing_the_second_time() {
        let mut dev = FakeDevice::stock();
        apply_profile(&mut dev, Profile::DeepIdleClamp).unwrap();
        dev.writes.clear();
        assert_eq!(apply_profile(&mut dev, Profile::DeepIdleClamp).unwrap(), 0);
        assert!(dev.writes.is_empty());
    }

    #[test]
    fn unknown_hook_state_refuses_before_any_write() {
        let mut dev = FakeDevice::stock();
        dev.mem[0x14B3] = 0x00;
        let err = apply_profile(&mut dev, Profile::IdlePowerDown).unwrap_err();
        assert!(matches!(err, Error::Refused(_)), "{err}");
        assert!(dev.writes.is_empty());
    }

    #[test]
    fn jmp_with_clamp_operands_but_vendor_body_is_unknown() {
        let mut dev = FakeDevice::stock();
        dev.mem[0x14B2..0x14B4].copy_from_slice(&[0xFC, 0x1E]);
        let s = read_state(&mut dev).unwrap();
        assert_eq!(s.hook_state(), None);
        assert!(plan(&s, Profile::Stock).is_err());
    }

    #[test]
    fn unknown_dac_routine_refuses_before_any_write() {
        let mut dev = FakeDevice::stock();
        dev.mem[0x17CA] = 0xEA;
        assert!(apply_profile(&mut dev, Profile::DeepIdleClamp).is_err());
        assert!(dev.writes.is_empty());
    }

    #[test]
    fn mic_boost_sets_value_and_rejects_out_of_range() {
        let mut dev = FakeDevice::stock();
        assert!(set_mic_boost(&mut dev, 6).unwrap());
        assert_eq!(dev.mem[0x1017], 0x3E);
        assert!(matches!(set_mic_boost(&mut dev, 8), Err(Error::Refused(_))));
        assert_eq!(dev.writes, vec![w(0x1017, "3e")]);
    }
}
