//! HID framing for CX2077x memory access, as used by fwupd's synaptics-cxaudio plugin.
//!
//! OUT report 0x04 (37 bytes): `[0x04, flags, length, addr_hi, addr_lo, data...]`.
//! IN report 0x05 (33 bytes): `[0x05, data...]`, fetched with GET_REPORT after a read request.

use crate::patches::Patch;
use crate::{Error, Result};

pub const VID: u16 = 0x0951;
pub const PID: u16 = 0x16A4;
pub const USAGE_PAGE: u16 = 0x000C;

pub const OUT_REPORT_ID: u8 = 0x04;
pub const IN_REPORT_ID: u8 = 0x05;
pub const OUT_LEN: usize = 37;
pub const IN_LEN: usize = 33;
pub const MAX_CHUNK: usize = 32;

/// Read CPU memory.
pub const FLAG_READ: u8 = 0x00;
/// Write CPU memory.
pub const FLAG_WRITE: u8 = 0x40;
/// Read EEPROM. Only ever sent alone: 0x20 together with 0x40 is an EEPROM write, so no code
/// path builds that combination. 0x10 (addr >= 64K) and 0x80 (DSP memory) are never used.
pub const FLAG_EEPROM_READ: u8 = 0x20;

const READ_ATTEMPTS: usize = 5;

/// One OUT report. It can only be built as a CPU-memory read request, an EEPROM read request
/// or from an allow-listed [`Patch`], so no code path can produce a write to an arbitrary
/// address or any request that writes the EEPROM.
pub struct OutReport([u8; OUT_LEN]);

impl OutReport {
    fn new(flags: u8, addr: u16, len: usize, data: &[u8]) -> Self {
        assert!(
            matches!(flags, FLAG_READ | FLAG_WRITE | FLAG_EEPROM_READ),
            "forbidden flags {flags:#04x}"
        );
        assert!((1..=MAX_CHUNK).contains(&len) && data.len() <= len);
        assert!(addr as usize + len <= 0x1_0000);
        let mut buf = [0u8; OUT_LEN];
        buf[0] = OUT_REPORT_ID;
        buf[1] = flags;
        buf[2] = len as u8;
        buf[3..5].copy_from_slice(&addr.to_be_bytes());
        buf[5..5 + data.len()].copy_from_slice(data);
        Self(buf)
    }

    fn read(addr: u16, len: usize) -> Self {
        Self::new(FLAG_READ, addr, len, &[])
    }

    /// An EEPROM read request: the flags are the constant [`FLAG_EEPROM_READ`] and there is no
    /// data, so it cannot be turned into a write.
    fn eeprom_read(addr: u16, len: usize) -> Self {
        Self::new(FLAG_EEPROM_READ, addr, len, &[])
    }

    /// Builds the write request of `data` for an allow-listed patch. Only
    /// [`crate::patches::apply_patch`] calls this, after its checks have passed.
    pub(crate) fn write(patch: &Patch, data: &[u8]) -> Self {
        assert_eq!(
            data.len(),
            patch.len(),
            "write of {} bytes for a patch of {}",
            data.len(),
            patch.len()
        );
        Self::new(FLAG_WRITE, patch.addr(), data.len(), data)
    }

    pub fn as_bytes(&self) -> &[u8; OUT_LEN] {
        &self.0
    }

    pub fn is_write(&self) -> bool {
        self.0[1] & FLAG_WRITE != 0
    }
}

/// Raw report exchange with the device. Implemented by the real HID device and by
/// the in-memory fake used in tests.
pub trait Transport {
    /// Sends one OUT report 0x04.
    fn send(&mut self, report: &OutReport) -> Result<()>;
    /// Fetches IN report 0x05 (GET_REPORT). Returns the report including its id byte.
    fn receive(&mut self) -> Result<Vec<u8>>;
}

/// Reads `len` bytes of CPU memory starting at `addr`, in chunks of at most 32 bytes.
pub fn read_mem<T: Transport + ?Sized>(t: &mut T, addr: u16, len: usize) -> Result<Vec<u8>> {
    read_range(t, addr, len, OutReport::read)
}

/// Reads `len` bytes of EEPROM starting at `addr`, in chunks of at most 32 bytes. A read only:
/// the request carries flag 0x20 alone.
pub fn read_eeprom<T: Transport + ?Sized>(t: &mut T, addr: u16, len: usize) -> Result<Vec<u8>> {
    read_range(t, addr, len, OutReport::eeprom_read)
}

fn read_range<T: Transport + ?Sized>(
    t: &mut T,
    addr: u16,
    len: usize,
    request: fn(u16, usize) -> OutReport,
) -> Result<Vec<u8>> {
    if len == 0 || addr as usize + len > 0x1_0000 {
        return Err(Error::Protocol(format!(
            "invalid read of {len} bytes at ${addr:04X}"
        )));
    }
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let n = (len - out.len()).min(MAX_CHUNK);
        let chunk_addr = addr + out.len() as u16;
        out.extend_from_slice(&read_chunk(t, &request(chunk_addr, n), chunk_addr, n)?);
    }
    Ok(out)
}

fn read_chunk<T: Transport + ?Sized>(
    t: &mut T,
    request: &OutReport,
    addr: u16,
    n: usize,
) -> Result<Vec<u8>> {
    for _ in 0..READ_ATTEMPTS {
        t.send(request)?;
        let r = t.receive()?;
        if r.len() > n && r[0] == IN_REPORT_ID {
            return Ok(r[1..=n].to_vec());
        }
    }
    Err(Error::Protocol(format!(
        "no valid IN report for read of {n} bytes at ${addr:04X}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeDevice;
    use crate::mode::Mode;
    use crate::patches::{apply_patch, settle_time};

    /// Forwards to the fake device and records the flags of every report sent.
    struct Recorder {
        device: FakeDevice,
        flags: Vec<u8>,
    }

    impl Recorder {
        fn new(device: FakeDevice) -> Self {
            Self {
                device,
                flags: Vec::new(),
            }
        }
    }

    impl Transport for Recorder {
        fn send(&mut self, report: &OutReport) -> Result<()> {
            self.flags.push(report.as_bytes()[1]);
            self.device.send(report)
        }

        fn receive(&mut self) -> Result<Vec<u8>> {
            self.device.receive()
        }
    }

    #[test]
    #[should_panic(expected = "forbidden flags 0x60")]
    fn an_eeprom_write_cannot_be_built() {
        OutReport::new(FLAG_EEPROM_READ | FLAG_WRITE, 0, 1, &[0xFF]);
    }

    #[test]
    #[should_panic(expected = "forbidden flags 0x60")]
    fn an_eeprom_write_cannot_be_built_without_data() {
        OutReport::new(FLAG_EEPROM_READ | FLAG_WRITE, 0, 1, &[]);
    }

    #[test]
    fn only_the_three_known_flag_values_are_accepted() {
        for flags in 0..=u8::MAX {
            let built = std::panic::catch_unwind(|| OutReport::new(flags, 0x1000, 1, &[]));
            let expected = [FLAG_READ, FLAG_EEPROM_READ, FLAG_WRITE].contains(&flags);
            assert_eq!(built.is_ok(), expected, "flags {flags:#04x}");
        }
    }

    #[test]
    fn an_eeprom_read_request_has_flag_0x20_alone_and_no_data() {
        let r = OutReport::eeprom_read(0x1234, 32);
        let b = r.as_bytes();
        assert_eq!(b[..5], [OUT_REPORT_ID, 0x20, 32, 0x12, 0x34]);
        assert!(b[5..].iter().all(|&x| x == 0));
        assert!(!r.is_write());
    }

    #[test]
    fn eeprom_reads_send_only_flag_0x20_and_memory_reads_only_flag_0x00() {
        let mut t = Recorder::new(FakeDevice::stock());
        assert_eq!(read_eeprom(&mut t, 0x0100, 70).unwrap().len(), 70);
        assert_eq!(t.flags, [0x20; 3]);
        t.flags.clear();
        assert_eq!(read_mem(&mut t, 0x1000, 70).unwrap().len(), 70);
        assert_eq!(t.flags, [0x00; 3]);
    }

    #[test]
    fn eeprom_reads_walk_the_addresses_in_chunks_of_32() {
        let device = FakeDevice::stock();
        let image: Vec<u8> = (0..0x4000).map(|i| (i % 251) as u8).collect();
        device.set_eeprom(&image);
        let mut t = device;
        assert_eq!(read_eeprom(&mut t, 0x0021, 100).unwrap(), image[0x21..0x85]);
        assert_eq!(read_eeprom(&mut t, 0x3FF0, 16).unwrap(), image[0x3FF0..]);
    }

    #[test]
    fn the_write_path_never_sends_the_eeprom_flag() {
        let mut t = Recorder::new(FakeDevice::in_mode(Mode::MinHiss));
        apply_patch(&mut t, &settle_time(0x20).unwrap()).unwrap();
        assert!(t.flags.contains(&FLAG_WRITE), "no write was sent");
        assert!(
            t.flags.iter().all(|f| f & FLAG_EEPROM_READ == 0),
            "{:02x?}",
            t.flags
        );
    }
}
