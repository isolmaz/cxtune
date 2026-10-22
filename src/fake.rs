//! In-memory stand-in for the dongle: it speaks the HID report protocol against a 64 KiB
//! CPU memory image. Its stock image is written from the documented firmware bytes,
//! independently of the patch table, so a typo in the table makes tests fail.

use crate::Result;
use crate::protocol::{OutReport, Transport};

pub struct FakeDevice {
    pub mem: Vec<u8>,
    /// Every write request received, as (address, data).
    pub writes: Vec<(u16, Vec<u8>)>,
    /// When set, write requests are recorded but do not change memory.
    pub ignore_writes: bool,
    pending: Option<Vec<u8>>,
}

pub fn bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

impl FakeDevice {
    /// Memory as the vendor patch leaves it after plug-in.
    pub fn stock() -> Self {
        let mut mem = vec![0u8; 0x1_0000];
        let mut put = |addr: usize, hex: &str| {
            let b = bytes(hex);
            mem[addr..addr + b.len()].copy_from_slice(&b);
        };
        put(0x14B1, "4c1b1f4c");
        put(0x16C9, "ad180f29df09038d180f");
        put(0x17CA, "ad180f29df09038d180f");
        put(0x1EFC, "2fbf1bcd8512f0168d85128dea13a9018d84128de913a9");
        put(0x1017, "3c");
        put(0x0F18, "03");
        put(0x0F1A, "00");
        Self {
            mem,
            writes: Vec::new(),
            ignore_writes: false,
            pending: None,
        }
    }
}

impl Transport for FakeDevice {
    fn send(&mut self, report: &OutReport) -> Result<()> {
        let b = report.as_bytes();
        assert_eq!(b[0], 0x04, "OUT report id");
        let len = b[2] as usize;
        let addr = u16::from_be_bytes([b[3], b[4]]) as usize;
        assert!((1..=32).contains(&len));
        match b[1] {
            0x00 => {
                assert!(b[5..].iter().all(|&x| x == 0), "data in read request");
                self.pending = Some(self.mem[addr..addr + len].to_vec());
            }
            0x40 => {
                let data = b[5..5 + len].to_vec();
                if !self.ignore_writes {
                    self.mem[addr..addr + len].copy_from_slice(&data);
                }
                self.writes.push((addr as u16, data));
            }
            flags => panic!("forbidden flags {flags:#04x}"),
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Vec<u8>> {
        let mut r = vec![0u8; 33];
        if let Some(data) = self.pending.take() {
            r[0] = 0x05;
            r[1..1 + data.len()].copy_from_slice(&data);
        }
        Ok(r)
    }
}
