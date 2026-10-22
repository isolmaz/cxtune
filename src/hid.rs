//! The real device: discovery and the HID transport (hidapi, native Windows backend).

use std::thread::sleep;
use std::time::Duration;

use hidapi::{DeviceInfo, HidApi, HidDevice};

use crate::protocol::{IN_LEN, IN_REPORT_ID, OutReport, PID, Transport, USAGE_PAGE, VID};
use crate::{Error, Result};

/// The device needs a moment to service a read request before GET_REPORT.
const READ_DELAY: Duration = Duration::from_millis(5);
const WRITE_DELAY: Duration = Duration::from_millis(50);

fn hid_err(e: hidapi::HidError) -> Error {
    Error::Device(e.to_string())
}

pub fn api() -> Result<HidApi> {
    HidApi::new().map_err(hid_err)
}

/// The dongle's HID collection with usage page 0x000C, if connected.
pub fn find(api: &HidApi) -> Option<&DeviceInfo> {
    api.device_list()
        .find(|d| d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == USAGE_PAGE)
}

pub struct HidTransport {
    dev: HidDevice,
}

pub fn open(api: &HidApi) -> Result<HidTransport> {
    let info = find(api).ok_or_else(|| {
        Error::Device(format!(
            "HyperX Cloud II dongle ({VID:04X}:{PID:04X}, usage page {USAGE_PAGE:#06x}) not found"
        ))
    })?;
    let dev = info.open_device(api).map_err(hid_err)?;
    Ok(HidTransport { dev })
}

impl Transport for HidTransport {
    fn send(&mut self, report: &OutReport) -> Result<()> {
        self.dev.write(report.as_bytes()).map_err(hid_err)?;
        if report.is_write() {
            sleep(WRITE_DELAY);
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Vec<u8>> {
        sleep(READ_DELAY);
        let mut buf = vec![0u8; IN_LEN];
        buf[0] = IN_REPORT_ID;
        let n = self.dev.get_input_report(&mut buf).map_err(hid_err)?;
        buf.truncate(n);
        Ok(buf)
    }
}
