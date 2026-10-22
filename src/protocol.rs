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
/// Write CPU memory. These two are the only flag values this app ever emits;
/// 0x10 (addr >= 64K), 0x20 (EEPROM) and 0x80 (DSP memory) are never used.
pub const FLAG_WRITE: u8 = 0x40;

const READ_ATTEMPTS: usize = 5;

/// One OUT report. It can only be built as a CPU-memory read request or from an
/// allow-listed [`Patch`], so no code path can produce a write to an arbitrary address.
pub struct OutReport([u8; OUT_LEN]);

impl OutReport {
    fn new(flags: u8, addr: u16, len: usize, data: &[u8]) -> Self {
        assert!(flags == FLAG_READ || flags == FLAG_WRITE, "forbidden flags");
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

    /// Builds the write request for an allow-listed patch. Only [`crate::patches::apply_patch`]
    /// calls this, after its checks have passed.
    pub(crate) fn write(patch: &Patch) -> Self {
        Self::new(FLAG_WRITE, patch.addr(), patch.after().len(), patch.after())
    }

    pub fn as_bytes(&self) -> &[u8; OUT_LEN] {
        &self.0
    }

    pub fn is_write(&self) -> bool {
        self.0[1] == FLAG_WRITE
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
    if len == 0 || addr as usize + len > 0x1_0000 {
        return Err(Error::Protocol(format!(
            "invalid read of {len} bytes at ${addr:04X}"
        )));
    }
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let n = (len - out.len()).min(MAX_CHUNK);
        let chunk_addr = addr + out.len() as u16;
        out.extend_from_slice(&read_chunk(t, chunk_addr, n)?);
    }
    Ok(out)
}

fn read_chunk<T: Transport + ?Sized>(t: &mut T, addr: u16, n: usize) -> Result<Vec<u8>> {
    let request = OutReport::read(addr, n);
    for _ in 0..READ_ATTEMPTS {
        t.send(&request)?;
        let r = t.receive()?;
        if r.len() > n && r[0] == IN_REPORT_ID {
            return Ok(r[1..=n].to_vec());
        }
    }
    Err(Error::Protocol(format!(
        "no valid IN report for read of {n} bytes at ${addr:04X}"
    )))
}
