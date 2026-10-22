//! `cxtune dump`: reads the dongle's ROM and EEPROM, read-only, and compares their SHA-256
//! values with those of the reference unit in `docs/firmware-research.md`.
//!
//! The ROM is read with flag 0x00 ([`read_mem_confirmed`]) and the EEPROM with flag 0x20 alone
//! ([`read_eeprom`]); neither request type can carry the write flag, and every chunk is read
//! twice. Nothing is sent anywhere: the dump goes to a new file in the directory the user names
//! and the report is printed.

use std::fmt::Write as _;
use std::io::Write as _;
use std::ops::Range;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::protocol::{EEPROM_SIZE, Transport, read_eeprom, read_mem_confirmed};
use crate::{Error, Result};

pub const ROM_START: u16 = 0x7000;
pub const ROM_LEN: usize = 0x9000;
/// The patch stream inside the EEPROM: `0x05BF..=0x1EE9`.
const PATCH_STREAM: Range<usize> = 0x05BF..0x1EEA;

/// Reference unit: ROM `CATP_V00.15.00.00`, EEPROM patch rev `0010`.
pub const REFERENCE: Reference<'static> = Reference {
    rom: "07029dfb4676ce28c1ece6252d45e298a1140b092cbf981b071a30372f08438a",
    eeprom: "f5d9ce77bc35db8c9e9df7f03e3b2eaf1d5f03cb2b9fa01d95fe25301a2b781c",
    patch: "bd386d7e51090be1cdf3b9e3458c64a36fdfbf4169e33ec80fd7ac53f90788fc",
};

/// SHA-256 values (lowercase hex) of the three items that are compared.
pub struct Reference<'a> {
    pub rom: &'a str,
    pub eeprom: &'a str,
    pub patch: &'a str,
}

pub struct Dump {
    pub rom: Vec<u8>,
    pub eeprom: Vec<u8>,
}

/// Reads ROM `$7000..$FFFF` and EEPROM `0x0000..0x3FFF`.
pub fn read<T: Transport + ?Sized>(t: &mut T) -> Result<Dump> {
    Ok(Dump {
        rom: read_mem_confirmed(t, ROM_START, ROM_LEN)?,
        eeprom: read_eeprom(t, 0, EEPROM_SIZE)?,
    })
}

pub fn sha256(data: &[u8]) -> String {
    Sha256::digest(data).iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

const FILES: [&str; 2] = ["rom.bin", "eeprom.bin"];

/// Fails if `dir` already holds a dump, so that this can be checked before the slow read.
pub fn ensure_free(dir: &Path) -> Result<()> {
    match FILES.iter().find(|name| dir.join(name).exists()) {
        Some(name) => Err(Error::Refused(format!(
            "{name} already exists in {}; use a new directory or remove the old dump",
            dir.display()
        ))),
        None => Ok(()),
    }
}

impl Dump {
    /// Writes `rom.bin` and `eeprom.bin` into `dir`, creating it if needed. An existing file is
    /// never overwritten.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let io = |path: &Path, e: std::io::Error| {
            Error::Config(format!("cannot write {}: {e}", path.display()))
        };
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
        ensure_free(dir)?;
        for (name, data) in FILES.into_iter().zip([&self.rom, &self.eeprom]) {
            let path = dir.join(name);
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| io(&path, e))?;
            file.write_all(data).map_err(|e| io(&path, e))?;
        }
        Ok(())
    }

    /// The hashes of the dump and what they say about the unit, against [`REFERENCE`].
    pub fn report(&self) -> String {
        self.report_against(&REFERENCE)
    }

    /// The same firmware means the ROM and the patch stream match. The rest of the EEPROM (the
    /// serial string, the config block the ROM rewrites, free areas) may differ per unit.
    pub fn report_against(&self, reference: &Reference) -> String {
        let items = [
            ("ROM", "CPU $7000..$FFFF", &self.rom[..], reference.rom),
            (
                "EEPROM",
                "0x0000..0x3FFF",
                &self.eeprom[..],
                reference.eeprom,
            ),
            (
                "Patch stream",
                "EEPROM 0x05BF..0x1EE9",
                &self.eeprom[PATCH_STREAM],
                reference.patch,
            ),
        ];
        let mut out = String::new();
        let mut same = [false; 3];
        for (i, (name, range, data, expected)) in items.iter().enumerate() {
            let hash = sha256(data);
            same[i] = hash == *expected;
            let _ = writeln!(
                out,
                "{name:<13}{range:<23}{:>6} bytes  {hash}  {}",
                data.len(),
                if same[i] { "same" } else { "DIFFERENT" }
            );
        }
        let [rom, eeprom, patch] = same;
        if rom && patch {
            out.push_str("same firmware as reference");
            if eeprom {
                out.push_str(" (the whole EEPROM matches too)\n");
            } else {
                out.push_str(
                    "; the rest of the EEPROM differs, which is normal (serial string, config \
                     block, free areas)\n",
                );
            }
        } else {
            out.push_str(
                "different from the reference firmware. Please report the three hashes above in \
                 an issue (see docs/firmware-research.md, section 9). Do not attach or upload \
                 rom.bin or eeprom.bin; cxtune uploads nothing.\n",
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeDevice;
    use crate::testdir::TempDir;

    /// A pseudo-random fill, so that a chunk read at the wrong address or in the wrong order
    /// changes the bytes.
    fn pattern(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 24) as u8
            })
            .collect()
    }

    fn device() -> (FakeDevice, Vec<u8>, Vec<u8>) {
        let dev = FakeDevice::stock();
        let rom = pattern(ROM_LEN, 1);
        let eeprom = pattern(EEPROM_SIZE, 2);
        dev.poke(ROM_START, &rom);
        dev.set_eeprom(&eeprom);
        (dev, rom, eeprom)
    }

    fn dumped() -> Dump {
        read(&mut device().0).unwrap()
    }

    fn hashes(dump: &Dump) -> [String; 3] {
        [
            sha256(&dump.rom),
            sha256(&dump.eeprom),
            sha256(&dump.eeprom[0x05BF..=0x1EE9]),
        ]
    }

    fn copy(dump: &Dump) -> Dump {
        Dump {
            rom: dump.rom.clone(),
            eeprom: dump.eeprom.clone(),
        }
    }

    #[test]
    fn the_dump_is_the_rom_and_the_eeprom_and_writes_nothing() {
        let (mut dev, rom, eeprom) = device();
        let before = dev.mem();
        let dump = read(&mut dev).unwrap();
        assert_eq!(dump.rom, rom);
        assert_eq!(dump.eeprom, eeprom);
        assert!(dev.writes().is_empty());
        assert_eq!(dev.mem(), before);
        // every chunk is read twice
        assert_eq!(dev.requests(), 2 * (ROM_LEN + EEPROM_SIZE) / 32);
        assert_eq!(dev.eeprom_reads(), 2 * EEPROM_SIZE / 32);
    }

    #[test]
    fn save_writes_exactly_the_two_files() {
        let (mut dev, rom, eeprom) = device();
        let dump = read(&mut dev).unwrap();
        let dir = TempDir::new("dump");
        let out = dir.join("nested").join("out");
        dump.save(&out).unwrap();
        assert_eq!(std::fs::read(out.join("rom.bin")).unwrap(), rom);
        assert_eq!(std::fs::read(out.join("eeprom.bin")).unwrap(), eeprom);
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 2);
    }

    #[test]
    fn an_existing_dump_is_never_overwritten() {
        let dump = dumped();
        for existing in ["rom.bin", "eeprom.bin"] {
            let dir = TempDir::new("dump");
            std::fs::write(dir.join(existing), "old").unwrap();
            assert!(matches!(ensure_free(&dir), Err(Error::Refused(_))));
            let err = dump.save(&dir).unwrap_err();
            assert!(
                matches!(err, Error::Refused(ref m) if m.contains(existing)),
                "{err}"
            );
            assert_eq!(std::fs::read(dir.join(existing)).unwrap(), b"old");
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        }
        let dir = TempDir::new("dump");
        ensure_free(&dir).unwrap();
        dump.save(&dir).unwrap();
        assert!(matches!(dump.save(&dir), Err(Error::Refused(_))));
    }

    #[test]
    fn save_reports_a_path_that_cannot_be_created() {
        let dir = TempDir::new("dump");
        let file = dir.join("a-file");
        std::fs::write(&file, "x").unwrap();
        let err = dumped().save(&file.join("out")).unwrap_err();
        assert!(
            matches!(err, Error::Config(ref m) if m.contains("a-file")),
            "{err}"
        );
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn the_reference_hashes_are_sha256_hex() {
        for h in [REFERENCE.rom, REFERENCE.eeprom, REFERENCE.patch] {
            assert_eq!(h.len(), 64);
            assert!(h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        }
    }

    #[test]
    fn a_dump_equal_to_the_reference_is_the_same_firmware() {
        let dump = dumped();
        let [rom, eeprom, patch] = hashes(&dump);
        let report = dump.report_against(&Reference {
            rom: &rom,
            eeprom: &eeprom,
            patch: &patch,
        });
        assert!(report.contains("same firmware as reference"), "{report}");
        assert!(!report.contains("DIFFERENT"), "{report}");
        for hash in [rom, eeprom, patch] {
            assert!(report.contains(&hash), "{report}");
        }
    }

    #[test]
    fn per_unit_eeprom_bytes_do_not_change_the_verdict() {
        let dump = dumped();
        let [rom, eeprom, patch] = hashes(&dump);
        let mut other = copy(&dump);
        other.eeprom[0x3000] ^= 0xFF;
        let report = other.report_against(&Reference {
            rom: &rom,
            eeprom: &eeprom,
            patch: &patch,
        });
        assert!(report.contains("same firmware as reference"), "{report}");
        assert!(report.contains("DIFFERENT"), "{report}");
    }

    #[test]
    fn a_changed_rom_or_patch_byte_is_a_different_firmware() {
        let dump = dumped();
        let [rom, eeprom, patch] = hashes(&dump);
        let reference = Reference {
            rom: &rom,
            eeprom: &eeprom,
            patch: &patch,
        };
        for (in_rom, at) in [
            (true, 0),
            (true, ROM_LEN - 1),
            (false, 0x05BF),
            (false, 0x1EE9),
        ] {
            let mut other = copy(&dump);
            if in_rom {
                other.rom[at] ^= 1;
            } else {
                other.eeprom[at] ^= 1;
            }
            let report = other.report_against(&reference);
            assert!(!report.contains("same firmware"), "{report}");
            assert!(
                report.contains("Please report the three hashes"),
                "{report}"
            );
            assert!(report.contains("uploads nothing"), "{report}");
            for hash in hashes(&other) {
                assert!(report.contains(&hash), "{report}");
            }
        }
    }
}
