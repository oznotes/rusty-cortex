//! EDL USB transport layer — implements qdlrs QdlReadWrite trait using nusb 0.1.
//!
//! Single-stream bulk I/O for Qualcomm EDL mode (VID:05C6/PID:9008).
//! No multiplexing needed (unlike ADB USB dispatcher).

use std::io::{self, BufRead, Read, Write};

use nusb::transfer::RequestBuffer;
use nusb::Interface;
use tracing::{debug, info};

use qdl::types::{FirehoseConfiguration, FirehoseStorageType, QdlBackend, QdlReadWrite};

const EDL_VID: u16 = 0x05C6;
const EDL_PID: u16 = 0x9008;
const EDL_INTERFACE_CLASS: u8 = 0xFF;
const EDL_INTERFACE_SUBCLASS: u8 = 0xFF;
const EDL_PROTOCOL_CODES: [u8; 3] = [0x10, 0x11, 0xFF];
const USB_READ_BUF: usize = 4096;
const BUF_SIZE: usize = 4096;

pub struct EdlUsbTransport {
    interface: Interface,
    ep_in: u8,
    ep_out: u8,
    buf: Vec<u8>,
    pos: usize,
    cap: usize,
}

impl EdlUsbTransport {
    /// Open the first EDL device found on USB.
    pub fn open() -> Result<Self, io::Error> {
        let device_info = nusb::list_devices()
            .map_err(|e| {
                io::Error::other(format!("USB enumeration failed: {e}"))
            })?
            .find(|d| d.vendor_id() == EDL_VID && d.product_id() == EDL_PID)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "No EDL device (05C6:9008) found")
            })?;

        info!(
            "Found EDL device: {:04x}:{:04x}",
            device_info.vendor_id(),
            device_info.product_id()
        );

        let device = device_info.open().map_err(|e| {
            let msg = if e.to_string().contains("in use") || e.to_string().contains("busy") || e.to_string().contains("Access") {
                format!(
                    "Cannot open EDL device — another connection or program is using it. \
                     Close any other EDL tools (QFIL, bkerler/edl) and retry. Error: {e}"
                )
            } else {
                format!(
                    "Cannot open EDL device. Install WinUSB driver via Zadig \
                     for 'Qualcomm HS-USB QDLoader 9008'. Error: {e}"
                )
            };
            io::Error::new(io::ErrorKind::PermissionDenied, msg)
        })?;

        let config = device.configurations().next().ok_or_else(|| {
            io::Error::other("No USB configuration found")
        })?;

        let mut intf_num = None;
        let mut ep_in = None;
        let mut ep_out = None;

        for intf in config.interfaces() {
            for alt in intf.alt_settings() {
                if alt.class() == EDL_INTERFACE_CLASS
                    && alt.subclass() == EDL_INTERFACE_SUBCLASS
                    && EDL_PROTOCOL_CODES.contains(&alt.protocol())
                {
                    intf_num = Some(intf.interface_number());
                    for ep in alt.endpoints() {
                        match ep.direction() {
                            nusb::transfer::Direction::In => ep_in = Some(ep.address()),
                            nusb::transfer::Direction::Out => ep_out = Some(ep.address()),
                        }
                    }
                    break;
                }
            }
            if intf_num.is_some() {
                break;
            }
        }

        let intf_num = intf_num.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "No EDL interface found (class FF/FF)",
            )
        })?;
        let ep_in = ep_in.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "No bulk IN endpoint found")
        })?;
        let ep_out = ep_out.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "No bulk OUT endpoint found")
        })?;

        let interface = device.claim_interface(intf_num).map_err(|e| {
            let msg = if e.to_string().contains("in use") || e.to_string().contains("busy") || e.to_string().contains("Access") {
                format!(
                    "Cannot claim EDL interface — device is already in use. \
                     If already connected in this app, disconnect first. \
                     Otherwise close other EDL tools (QFIL, bkerler/edl). Error: {e}"
                )
            } else {
                format!(
                    "Cannot claim EDL interface. Install WinUSB driver via Zadig \
                     for 'Qualcomm HS-USB QDLoader 9008' (replace QDLoader with WinUSB). Error: {e}"
                )
            };
            io::Error::new(io::ErrorKind::PermissionDenied, msg)
        })?;

        debug!(
            "EDL USB: interface {}, ep_in 0x{:02x}, ep_out 0x{:02x}",
            intf_num, ep_in, ep_out
        );

        Ok(Self {
            interface,
            ep_in,
            ep_out,
            buf: vec![0u8; BUF_SIZE],
            pos: 0,
            cap: 0,
        })
    }
}

impl Read for EdlUsbTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Drain internal buffer first
        if self.pos < self.cap {
            let avail = &self.buf[self.pos..self.cap];
            let n = avail.len().min(buf.len());
            buf[..n].copy_from_slice(&avail[..n]);
            self.pos += n;
            return Ok(n);
        }

        let req = RequestBuffer::new(buf.len().max(64));
        let completion = pollster::block_on(self.interface.bulk_in(self.ep_in, req));
        let data = completion.into_result().map_err(|e| {
            io::Error::other(format!("USB bulk_in failed: {e}"))
        })?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        // Store excess data in internal buffer to avoid data loss
        if data.len() > n {
            let excess = &data[n..];
            let store = excess.len().min(self.buf.len());
            self.buf[..store].copy_from_slice(&excess[..store]);
            self.pos = 0;
            self.cap = store;
        }
        Ok(n)
    }
}

impl Write for EdlUsbTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        pollster::block_on(self.interface.bulk_out(self.ep_out, buf.to_vec()))
            .into_result()
            .map_err(|e| {
                io::Error::other(format!("USB bulk_out failed: {e}"))
            })?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl BufRead for EdlUsbTransport {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos >= self.cap {
            let req = RequestBuffer::new(USB_READ_BUF);
            let completion = pollster::block_on(self.interface.bulk_in(self.ep_in, req));
            let data = completion.into_result().map_err(|e| {
                io::Error::other(format!("USB fill_buf failed: {e}"))
            })?;
            let n = data.len().min(self.buf.len());
            self.buf[..n].copy_from_slice(&data[..n]);
            self.pos = 0;
            self.cap = n;
        }
        Ok(&self.buf[self.pos..self.cap])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.cap);
    }
}

// nusb::Interface is Arc-backed (Clone + Send + Sync internally).
unsafe impl Send for EdlUsbTransport {}
unsafe impl Sync for EdlUsbTransport {}

impl QdlReadWrite for EdlUsbTransport {}

/// Create default FirehoseConfiguration for our transport.
pub fn default_firehose_config() -> FirehoseConfiguration {
    FirehoseConfiguration {
        send_buffer_size: 1024 * 1024,
        recv_buffer_size: 4096,
        xml_buf_size: 4096,
        // Default to UFS — most modern Qualcomm SoCs (SDM845+) use UFS.
        // After connect, LUN probe auto-detects: >1 LUN = UFS, 1 LUN = eMMC fallback.
        storage_sector_size: 4096,
        storage_type: FirehoseStorageType::Ufs,
        bypass_storage: false,
        hash_packets: false,
        read_back_verify: false,
        backend: QdlBackend::Usb,
        skip_firehose_log: false,
        verbose_firehose: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bypass_storage_default_false() {
        let cfg = default_firehose_config();
        assert!(!cfg.bypass_storage, "bypass_storage must be false for write operations to work");
    }
}
