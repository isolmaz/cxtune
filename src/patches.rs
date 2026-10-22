//! The allow-list of every write this app can make, and the single function that performs them.
//!
//! Each [`Patch`] is a static (address, accepted-before-bytes, new-bytes, guards) entry.
//! Patches can only be defined in this module, so every write the app can ever send
//! corresponds to an entry here. All addresses are CPU-space patch RAM, which the
//! dongle reloads from EEPROM on every replug or USB reset.

use crate::protocol::{OutReport, Transport, read_mem};
use crate::{Error, Result, hex};

/// Bytes that must be present at `addr` before a patch may be written.
pub struct Guard {
    pub addr: u16,
    pub bytes: &'static [u8],
}

pub struct Patch {
    name: &'static str,
    addr: u16,
    before: &'static [&'static [u8]],
    after: &'static [u8],
    guards: &'static [Guard],
}

impl Patch {
    pub fn name(&self) -> &'static str {
        self.name
    }
    pub fn addr(&self) -> u16 {
        self.addr
    }
    pub fn after(&self) -> &'static [u8] {
        self.after
    }
}

/// Address ranges (start, length) that may ever be written. Checked on every write as a
/// second line of defence behind the static table.
const WRITABLE: [(u16, usize); 5] = [
    (HOOK, 3),
    (DAC_A, 1),
    (DAC_B, 1),
    (CLAMP_BODY, 23),
    (MIC_CONTROL, 1),
];

pub const HOOK: u16 = 0x14B1;
pub const HOOK_OPERANDS: u16 = 0x14B2;
pub const DAC_A_ROUTINE: u16 = 0x16C9;
pub const DAC_B_ROUTINE: u16 = 0x17CA;
pub const DAC_A: u16 = DAC_A_ROUTINE + 6;
pub const DAC_B: u16 = DAC_B_ROUTINE + 6;
pub const CLAMP_BODY: u16 = 0x1EFC;
pub const MIC_CONTROL: u16 = 0x1017;

pub const JMP: u8 = 0x4C;
pub const RTS: u8 = 0x60;
pub const VENDOR_OPERANDS: [u8; 2] = [0x1B, 0x1F];
pub const CLAMP_OPERANDS: [u8; 2] = [0xFC, 0x1E];
/// `LDA $0F18; AND #$DF; ORA #` precedes, and `STA $0F18` follows, the DAC-hold immediate.
pub const DAC_PREFIX: [u8; 6] = [0xAD, 0x18, 0x0F, 0x29, 0xDF, 0x09];
pub const DAC_SUFFIX: [u8; 3] = [0x8D, 0x18, 0x0F];
pub const DAC_HOLD_ON: u8 = 0x03;
pub const DAC_HOLD_OFF: u8 = 0x00;

/// Vendor patch bytes at $1EFC, replaced by the clamp routine.
pub const BODY_ORIG: [u8; 23] = [
    0x2F, 0xBF, 0x1B, 0xCD, 0x85, 0x12, 0xF0, 0x16, 0x8D, 0x85, 0x12, 0x8D, 0xEA, 0x13, 0xA9, 0x01,
    0x8D, 0x84, 0x12, 0x8D, 0xE9, 0x13, 0xA9,
];
/// If both DACs are idle, pop the caller and park the headphone driver at $0F1A=0x77.
pub const BODY_CLAMP: [u8; 23] = [
    0xAD, 0x18, 0x0F, 0x29, 0x03, 0xD0, 0x0F, 0x68, 0x68, 0xA9, 0x77, 0x8D, 0x1A, 0x0F, 0xAD, 0x1B,
    0x0F, 0x09, 0x33, 0x8D, 0x1B, 0x0F, 0x60,
];

pub const MIC_MIN: u8 = 0x38;
pub const MIC_MAX: u8 = 0x3F;

const VENDOR_HOOK_TAIL: [u8; 3] = [0x1B, 0x1F, 0x4C];
const CLAMP_HOOK_TAIL: [u8; 3] = [0xFC, 0x1E, 0x4C];
const HOOK_RTS_VENDOR_FULL: [u8; 4] = [0x60, 0x1B, 0x1F, 0x4C];

pub const HOOK_RTS_VENDOR: Patch = Patch {
    name: "hook30 JMP->RTS (vendor operands)",
    addr: HOOK,
    before: &[&[JMP]],
    after: &[RTS],
    guards: &[Guard {
        addr: HOOK_OPERANDS,
        bytes: &VENDOR_HOOK_TAIL,
    }],
};

pub const HOOK_JMP_VENDOR: Patch = Patch {
    name: "hook30 RTS->JMP (vendor operands)",
    addr: HOOK,
    before: &[&[RTS]],
    after: &[JMP],
    guards: &[Guard {
        addr: HOOK_OPERANDS,
        bytes: &VENDOR_HOOK_TAIL,
    }],
};

pub const BODY_INSTALL: Patch = Patch {
    name: "install clamp routine at $1EFC",
    addr: CLAMP_BODY,
    before: &[&BODY_ORIG],
    after: &BODY_CLAMP,
    guards: &[Guard {
        addr: HOOK,
        bytes: &HOOK_RTS_VENDOR_FULL,
    }],
};

pub const BODY_REMOVE: Patch = Patch {
    name: "restore vendor bytes at $1EFC",
    addr: CLAMP_BODY,
    before: &[&BODY_CLAMP],
    after: &BODY_ORIG,
    guards: &[Guard {
        addr: HOOK,
        bytes: &HOOK_RTS_VENDOR_FULL,
    }],
};

pub const OPERANDS_CLAMP: Patch = Patch {
    name: "hook30 operands -> $1EFC",
    addr: HOOK_OPERANDS,
    before: &[&VENDOR_OPERANDS],
    after: &CLAMP_OPERANDS,
    guards: &[
        Guard {
            addr: HOOK,
            bytes: &[RTS],
        },
        Guard {
            addr: CLAMP_BODY,
            bytes: &BODY_CLAMP,
        },
    ],
};

pub const OPERANDS_VENDOR: Patch = Patch {
    name: "hook30 operands -> vendor $1F1B",
    addr: HOOK_OPERANDS,
    before: &[&CLAMP_OPERANDS],
    after: &VENDOR_OPERANDS,
    guards: &[Guard {
        addr: HOOK,
        bytes: &[RTS],
    }],
};

pub const HOOK_JMP_CLAMP: Patch = Patch {
    name: "hook30 RTS->JMP (clamp routine)",
    addr: HOOK,
    before: &[&[RTS]],
    after: &[JMP],
    guards: &[
        Guard {
            addr: HOOK_OPERANDS,
            bytes: &CLAMP_HOOK_TAIL,
        },
        Guard {
            addr: CLAMP_BODY,
            bytes: &BODY_CLAMP,
        },
    ],
};

pub const HOOK_RTS_CLAMP: Patch = Patch {
    name: "hook30 JMP->RTS (clamp operands)",
    addr: HOOK,
    before: &[&[JMP]],
    after: &[RTS],
    guards: &[Guard {
        addr: HOOK_OPERANDS,
        bytes: &CLAMP_HOOK_TAIL,
    }],
};

const DAC_A_GUARDS: [Guard; 2] = [
    Guard {
        addr: DAC_A_ROUTINE,
        bytes: &DAC_PREFIX,
    },
    Guard {
        addr: DAC_A + 1,
        bytes: &DAC_SUFFIX,
    },
];
const DAC_B_GUARDS: [Guard; 2] = [
    Guard {
        addr: DAC_B_ROUTINE,
        bytes: &DAC_PREFIX,
    },
    Guard {
        addr: DAC_B + 1,
        bytes: &DAC_SUFFIX,
    },
];

pub const DAC_A_RELEASE: Patch = Patch {
    name: "DAC hold A off ($16CF)",
    addr: DAC_A,
    before: &[&[DAC_HOLD_ON]],
    after: &[DAC_HOLD_OFF],
    guards: &DAC_A_GUARDS,
};
pub const DAC_A_HOLD: Patch = Patch {
    name: "DAC hold A on ($16CF)",
    addr: DAC_A,
    before: &[&[DAC_HOLD_OFF]],
    after: &[DAC_HOLD_ON],
    guards: &DAC_A_GUARDS,
};
pub const DAC_B_RELEASE: Patch = Patch {
    name: "DAC hold B off ($17D0)",
    addr: DAC_B,
    before: &[&[DAC_HOLD_ON]],
    after: &[DAC_HOLD_OFF],
    guards: &DAC_B_GUARDS,
};
pub const DAC_B_HOLD: Patch = Patch {
    name: "DAC hold B on ($17D0)",
    addr: DAC_B,
    before: &[&[DAC_HOLD_OFF]],
    after: &[DAC_HOLD_ON],
    guards: &DAC_B_GUARDS,
};

/// Hook states in order: 0 vendor JMP, 1 vendor RTS, 2 body installed (RTS),
/// 3 clamp operands (RTS), 4 clamp JMP. `HOOK_FORWARD[i]` moves from state i to i+1.
pub const HOOK_FORWARD: [&Patch; 4] = [
    &HOOK_RTS_VENDOR,
    &BODY_INSTALL,
    &OPERANDS_CLAMP,
    &HOOK_JMP_CLAMP,
];
/// `HOOK_BACKWARD[i]` moves from hook state i+1 to i.
pub const HOOK_BACKWARD: [&Patch; 4] = [
    &HOOK_JMP_VENDOR,
    &BODY_REMOVE,
    &OPERANDS_VENDOR,
    &HOOK_RTS_CLAMP,
];

const MIC_VALUES: [&[u8]; 8] = [
    &[0x38],
    &[0x39],
    &[0x3A],
    &[0x3B],
    &[0x3C],
    &[0x3D],
    &[0x3E],
    &[0x3F],
];

const fn mic(boost: usize) -> Patch {
    Patch {
        name: "MIC_CONTROL boost ($1017)",
        addr: MIC_CONTROL,
        before: &MIC_VALUES,
        after: MIC_VALUES[boost],
        guards: &[],
    }
}

/// `MIC_BOOST[n]` sets $1017 to 0x38 + n (analog mic boost n, 6 dB steps).
pub const MIC_BOOST: [Patch; 8] = [
    mic(0),
    mic(1),
    mic(2),
    mic(3),
    mic(4),
    mic(5),
    mic(6),
    mic(7),
];

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Written,
    AlreadySet,
}

/// The only function in the app that writes to the device.
///
/// Reads the target bytes; returns `AlreadySet` if they already hold the new value;
/// refuses unless they hold an accepted before-value and every guard matches; writes;
/// reads back and fails if the device does not hold the new bytes.
pub fn apply_patch<T: Transport + ?Sized>(t: &mut T, p: &Patch) -> Result<Outcome> {
    let len = p.after.len();
    let in_region = WRITABLE
        .iter()
        .any(|&(start, n)| p.addr >= start && p.addr as usize + len <= start as usize + n);
    if !in_region || p.before.iter().any(|b| b.len() != len) {
        return Err(Error::Refused(format!(
            "'{}' at ${:04X} is outside the writable patch-RAM regions",
            p.name, p.addr
        )));
    }

    let current = read_mem(t, p.addr, len)?;
    if current == p.after {
        return Ok(Outcome::AlreadySet);
    }
    if !p.before.contains(&current.as_slice()) {
        return Err(Error::Refused(format!(
            "'{}': ${:04X} holds {}, not an expected value",
            p.name,
            p.addr,
            hex(&current)
        )));
    }
    for g in p.guards {
        let got = read_mem(t, g.addr, g.bytes.len())?;
        if got != g.bytes {
            return Err(Error::Refused(format!(
                "'{}': guard ${:04X} holds {}, expected {}",
                p.name,
                g.addr,
                hex(&got),
                hex(g.bytes)
            )));
        }
    }

    t.send(&OutReport::write(p))?;

    let got = read_mem(t, p.addr, len)?;
    if got != p.after {
        return Err(Error::Readback {
            patch: p.name,
            addr: p.addr,
            expected: p.after.to_vec(),
            got,
        });
    }
    Ok(Outcome::Written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeDevice;

    fn all() -> Vec<&'static Patch> {
        let mut v = vec![
            &HOOK_RTS_VENDOR,
            &HOOK_JMP_VENDOR,
            &BODY_INSTALL,
            &BODY_REMOVE,
            &OPERANDS_CLAMP,
            &OPERANDS_VENDOR,
            &HOOK_JMP_CLAMP,
            &HOOK_RTS_CLAMP,
            &DAC_A_RELEASE,
            &DAC_A_HOLD,
            &DAC_B_RELEASE,
            &DAC_B_HOLD,
        ];
        v.extend(MIC_BOOST.iter());
        v
    }

    #[test]
    fn write_reports_are_cpu_ram_writes_to_allowed_addresses() {
        let allowed: [(u16, usize); 6] = [
            (0x14B1, 1),
            (0x14B2, 2),
            (0x16CF, 1),
            (0x17D0, 1),
            (0x1EFC, 23),
            (0x1017, 1),
        ];
        for p in all() {
            let r = OutReport::write(p);
            let b = r.as_bytes();
            assert_eq!(b[0], 0x04, "{}", p.name);
            assert_eq!(b[1], 0x40, "{}", p.name);
            let addr = u16::from_be_bytes([b[3], b[4]]);
            let len = b[2] as usize;
            assert!(
                allowed.contains(&(addr, len)),
                "{} -> ${addr:04X}/{len}",
                p.name
            );
            assert_eq!(&b[5..5 + len], p.after);
            assert!(b[5 + len..].iter().all(|&x| x == 0));
            if addr == 0x1017 {
                let values: Vec<u8> = p.before.iter().flat_map(|b| b.iter().copied()).collect();
                assert_eq!(values, (0x38..=0x3F).collect::<Vec<u8>>());
            } else {
                assert!(p.before.len() == 1 && p.before[0] != p.after, "{}", p.name);
            }
        }
    }

    #[test]
    fn table_bytes_match_known_firmware_values() {
        assert_eq!(
            hex(&BODY_ORIG).replace(' ', ""),
            "2fbf1bcd8512f0168d85128dea13a9018d84128de913a9"
        );
        assert_eq!(
            hex(&BODY_CLAMP).replace(' ', ""),
            "ad180f2903d00f6868a9778d1a0fad1b0f09338d1b0f60"
        );
        assert_eq!((DAC_A, DAC_B), (0x16CF, 0x17D0));
        for (n, p) in MIC_BOOST.iter().enumerate() {
            assert_eq!(p.after, [0x38 + n as u8]);
        }
    }

    #[test]
    fn refuses_when_target_bytes_are_unexpected() {
        let mut dev = FakeDevice::stock();
        dev.mem[0x16CF] = 0x05;
        let before = dev.mem.clone();
        assert!(matches!(
            apply_patch(&mut dev, &DAC_A_RELEASE),
            Err(Error::Refused(_))
        ));
        assert!(dev.writes.is_empty());
        assert_eq!(dev.mem, before);
    }

    #[test]
    fn refuses_when_dac_routine_context_differs() {
        for corrupt in [0x16C9, 0x16CE, 0x16D0, 0x16D2] {
            let mut dev = FakeDevice::stock();
            dev.mem[corrupt] ^= 0xFF;
            assert!(
                matches!(
                    apply_patch(&mut dev, &DAC_A_RELEASE),
                    Err(Error::Refused(_))
                ),
                "corrupt ${corrupt:04X}"
            );
            assert!(dev.writes.is_empty());
        }
    }

    #[test]
    fn refuses_to_install_body_while_hook_is_live() {
        let mut dev = FakeDevice::stock();
        assert!(matches!(
            apply_patch(&mut dev, &BODY_INSTALL),
            Err(Error::Refused(_))
        ));
        assert!(dev.writes.is_empty());
    }

    #[test]
    fn refuses_to_jump_into_clamp_operands_without_body() {
        let mut dev = FakeDevice::stock();
        dev.mem[0x14B1..0x14B5].copy_from_slice(&[0x60, 0xFC, 0x1E, 0x4C]);
        assert!(matches!(
            apply_patch(&mut dev, &HOOK_JMP_CLAMP),
            Err(Error::Refused(_))
        ));
        assert!(dev.writes.is_empty());
    }

    #[test]
    fn refuses_to_change_operands_while_hook_is_jmp() {
        let mut dev = FakeDevice::stock();
        dev.mem[0x1EFC..0x1EFC + 23].copy_from_slice(&BODY_CLAMP);
        assert!(matches!(
            apply_patch(&mut dev, &OPERANDS_CLAMP),
            Err(Error::Refused(_))
        ));
        assert!(dev.writes.is_empty());
    }

    #[test]
    fn refuses_mic_value_outside_allowed_range() {
        let mut dev = FakeDevice::stock();
        dev.mem[0x1017] = 0x07;
        assert!(matches!(
            apply_patch(&mut dev, &MIC_BOOST[6]),
            Err(Error::Refused(_))
        ));
        assert!(dev.writes.is_empty());
    }

    #[test]
    fn refuses_patch_outside_writable_regions() {
        let rogue = Patch {
            name: "rogue",
            addr: 0x1151,
            before: &[&[0x00]],
            after: &[0x10],
            guards: &[],
        };
        let mut dev = FakeDevice::stock();
        dev.mem[0x1151] = 0x00;
        assert!(matches!(
            apply_patch(&mut dev, &rogue),
            Err(Error::Refused(_))
        ));
        assert!(dev.writes.is_empty());
        assert_eq!(dev.mem[0x1151], 0x00);
    }

    #[test]
    fn readback_mismatch_is_an_error() {
        let mut dev = FakeDevice::stock();
        dev.ignore_writes = true;
        let err = apply_patch(&mut dev, &MIC_BOOST[6]).unwrap_err();
        assert!(matches!(err, Error::Readback { addr: 0x1017, .. }), "{err}");
        assert_eq!(dev.writes, vec![(0x1017, vec![0x3E])]);
    }

    #[test]
    fn writes_new_bytes_after_checks_pass() {
        let mut dev = FakeDevice::stock();
        assert_eq!(
            apply_patch(&mut dev, &DAC_B_RELEASE).unwrap(),
            Outcome::Written
        );
        assert_eq!(dev.writes, vec![(0x17D0, vec![0x00])]);
        assert_eq!(dev.mem[0x17D0], 0x00);
    }

    #[test]
    fn already_set_value_is_not_rewritten() {
        let mut dev = FakeDevice::stock();
        assert_eq!(
            apply_patch(&mut dev, &MIC_BOOST[4]).unwrap(),
            Outcome::AlreadySet
        );
        assert!(dev.writes.is_empty());
    }
}
