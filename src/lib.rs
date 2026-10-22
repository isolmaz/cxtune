//! Core of cxtune: HID memory access to the CX20773 codec in the HyperX Cloud II
//! USB dongle, an allow-listed table of volatile patch-RAM writes, and profiles
//! built from that table.
//!
//! All writes go through [`patches::apply_patch`]; there is no API that writes an
//! arbitrary address. Nothing here touches EEPROM or DSP memory.

pub mod config;
pub mod hid;
pub mod patches;
pub mod profile;
pub mod protocol;

#[cfg(test)]
mod fake;

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The device could not be found, opened or talked to.
    Device(String),
    /// A response from the device did not follow the protocol.
    Protocol(String),
    /// A safety check failed; nothing was written by the failing step.
    Refused(String),
    /// A write was sent but the device memory does not hold the new bytes.
    Readback {
        patch: &'static str,
        addr: u16,
        expected: Vec<u8>,
        got: Vec<u8>,
    },
    Config(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Device(m) => write!(f, "device error: {m}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Refused(m) => write!(f, "refused: {m}"),
            Error::Readback {
                patch,
                addr,
                expected,
                got,
            } => write!(
                f,
                "READBACK MISMATCH after '{patch}' at ${addr:04X}: expected {}, device has {}. \
                 Replug the dongle to restore the stock patch.",
                hex(expected),
                hex(got)
            ),
            Error::Config(m) => write!(f, "config error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Formats bytes as lowercase hex pairs separated by spaces.
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
