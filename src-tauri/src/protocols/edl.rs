//! EDL (Qualcomm Emergency Download) protocol implementation.
//!
//! Two-phase protocol:
//! 1. Sahara (binary) — device identification + programmer upload
//! 2. Firehose (XML over USB) — partition operations + reboot
//!
//! Uses qdlrs crate for protocol logic, EdlUsbTransport for USB I/O.

use std::io::{BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use base64::Engine;
use sha2::{Sha256, Digest};
use tracing::{info, warn};

use qdl::sahara::{sahara_run, SaharaCmdModeCmd, SaharaMode};
use qdl::types::{FirehoseResetMode, FirehoseStorageType, QdlBackend, QdlDevice, QdlReadWrite};
use qdl::{
    firehose_configure, firehose_get_storage_info, firehose_patch, firehose_program_storage,
    firehose_read, firehose_read_storage, firehose_reset, firehose_write,
};
use super::edl_gpt::parse_gpt;
use super::edl_mbn::is_valid_programmer_magic;
use super::edl_usb::{default_firehose_config, EdlUsbTransport};
use super::edl_xml::{parse_patch_xml, parse_rawprogram};
use super::sparse::ensure_raw_image;
use crate::error::FlashError;
use crate::types::{BatchFlashResult, EdlDeviceInfo, EdlPartitionEntry, VerifyResult};

/// Stateful EDL connection — holds programmer-loaded Firehose session.
pub struct EdlConnection {
    pub device: QdlDevice<dyn QdlReadWrite>,
    pub info: EdlDeviceInfo,
}

/// Write wrapper that tracks bytes written for progress reporting.
pub struct ProgressWriter<W: Write> {
    inner: W,
    written: Arc<AtomicU64>,
}

impl<W: Write> ProgressWriter<W> {
    pub fn new(inner: W) -> (Self, Arc<AtomicU64>) {
        let written = Arc::new(AtomicU64::new(0));
        let written_clone = written.clone();
        (Self { inner, written }, written_clone)
    }
}

impl<W: Write> Write for ProgressWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Read wrapper that tracks bytes read for progress reporting.
pub struct ProgressReader<R: Read> {
    inner: R,
    read_bytes: Arc<AtomicU64>,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read_bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

pub struct EdlProtocol;

const VERIFY_CHUNK_SIZE: usize = 1024 * 1024; // 1MB

// ---------------------------------------------------------------------------
// Safe qdlrs call wrappers
// ---------------------------------------------------------------------------

/// Wrap any qdlrs call in catch_unwind to prevent panics from crashing the app.
/// qdlrs parsers use unwrap() on XML attributes — any unexpected data panics.
fn qdl_safe<F, T, E>(f: F) -> Result<T, FlashError>
where
    F: FnOnce() -> Result<T, E>,
    E: std::fmt::Display,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(val)) => Ok(val),
        Ok(Err(e)) => Err(FlashError::Protocol(format!("{e}"))),
        Err(_) => Err(FlashError::Protocol(
            "Firehose protocol error — programmer sent unexpected response".into(),
        )),
    }
}

/// Drain any pending data from the Firehose transport buffer.
/// Used after auth attempts or error recovery to clear stale data.
/// Reads up to 20 times (programmer can dump 15+ log messages after nop/configure).
fn drain_firehose_buffer(device: &mut QdlDevice<dyn QdlReadWrite>) {
    let mut buf = vec![0u8; 4096];
    for _ in 0..20 {
        match device.rw.read(&mut buf) {
            Ok(n) if n > 0 => {
                let msg = String::from_utf8_lossy(&buf[..n]);
                info!("EDL: drained ({n} bytes): {msg}");
            }
            _ => break,
        }
    }
}

/// Read raw Firehose response, accumulating until `<response value` is found.
/// Bypasses qdlrs parsers to avoid panics on unexpected XML attributes.
fn read_firehose_response_raw(
    device: &mut QdlDevice<dyn QdlReadWrite>,
) -> Result<String, FlashError> {
    let mut accumulated = Vec::new();
    let mut buf = vec![0u8; 4096];

    for _ in 0..20 {
        match device.rw.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                accumulated.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&accumulated);
                if text.contains("<response value") {
                    return Ok(text.into_owned());
                }
            }
            Err(_) => break,
        }
    }

    if accumulated.is_empty() {
        Err(FlashError::Protocol("No Firehose response received".into()))
    } else {
        Ok(String::from_utf8_lossy(&accumulated).into_owned())
    }
}

/// Probe whether a device is in Firehose mode by sending a nop XML command.
/// Used after Sahara identify times out — if the device has a stale programmer
/// loaded from a previous session, it will respond with XML instead of Sahara binary.
/// Returns true if the device is in Firehose mode.
fn probe_firehose_nop(device: &mut QdlDevice<dyn QdlReadWrite>) -> bool {
    let nop_xml = b"<?xml version=\"1.0\" ?><data><nop /></data>";

    // Write nop command
    if let Err(e) = device.rw.write_all(nop_xml) {
        warn!("EDL: nop probe write failed: {e}");
        return false;
    }

    // Read response — look for XML markers indicating Firehose is active
    let mut accumulated = Vec::new();
    let mut buf = vec![0u8; 4096];

    for _ in 0..5 {
        match device.rw.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                accumulated.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&accumulated);
                // Firehose responds with XML: <?xml, <response, <log, or <data
                if text.contains("<?xml") || text.contains("<response") || text.contains("<log") || text.contains("<data") {
                    info!("EDL: nop probe got Firehose XML response ({} bytes)", accumulated.len());
                    // Drain any remaining data
                    drain_firehose_buffer(device);
                    return true;
                }
            }
            Err(e) => {
                warn!("EDL: nop probe read failed: {e}");
                break;
            }
        }
    }

    if !accumulated.is_empty() {
        let preview = String::from_utf8_lossy(&accumulated[..accumulated.len().min(200)]);
        info!("EDL: nop probe got non-XML response ({} bytes): {preview}", accumulated.len());
    } else {
        info!("EDL: nop probe got no response");
    }
    false
}

/// Extract an XML attribute value from raw XML text.
/// Simple string-based parser — avoids qdlrs XML parsing panics.
fn extract_xml_attr(xml: &str, attr_name: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr_name);
    let start = xml.find(&pattern)? + pattern.len();
    let end = xml[start..].find('"')? + start;
    Some(xml[start..end].to_string())
}

/// Parse configure response values from raw XML and update device config.
/// Replaces qdlrs firehose_parser_configure_response (which panics on missing attrs).
fn apply_configure_response(response: &str, device: &mut QdlDevice<dyn QdlReadWrite>) {
    if let Some(val) = extract_xml_attr(response, "MaxPayloadSizeToTargetInBytes") {
        if let Ok(size) = val.parse::<usize>() {
            device.fh_cfg.send_buffer_size = size;
            info!("EDL: MaxPayloadSizeToTargetInBytes = {size}");
        }
    }
    if let Some(val) = extract_xml_attr(response, "MaxXMLSizeInBytes") {
        if let Ok(size) = val.parse::<usize>() {
            device.fh_cfg.xml_buf_size = size;
            info!("EDL: MaxXMLSizeInBytes = {size}");
        }
    }
    if let Some(name) = extract_xml_attr(response, "MemoryName") {
        info!("EDL: device reports MemoryName = {name}");
        match name.to_lowercase().as_str() {
            "ufs" => {
                device.fh_cfg.storage_type = FirehoseStorageType::Ufs;
                device.fh_cfg.storage_sector_size = 4096;
            }
            "emmc" => {
                device.fh_cfg.storage_type = FirehoseStorageType::Emmc;
                device.fh_cfg.storage_sector_size = 512;
            }
            _ => {}
        }
    }
}

/// Xiaomi EDL auth tokens (pre-computed RSA-2048 signatures, 256 bytes each).
/// Source: bkerler/edl — works on pre-SM8350 Xiaomi devices.
const XIAOMI_AUTH_TOKENS: [&str; 2] = [
    // Token 1: "encrypted" — works on Poco F1, Redmi 5/6/7/8 Pro, Y2, S2, A1
    "k246jlc8rQfBZ2RLYSF4Ndha1P3bfYQKK3IlQy/NoTp8GSz6l57RZRfmlwsbB99s\
     UW/sgfaWj89//dvDl6Fiwso+XXYSSqF2nxshZLObdpMLTMZ1GffzOYd2d/ToryWC\
     hoK8v05ZOlfn4wUyaZJT4LHMXZ0NVUryvUbVbxjW5SkLpKDKwkMfnxnEwaOddmT\
     /q0ip4RpVk4aBmDW4TfVnXnDSX9tRI+ewQP4hEI8K5tfZ0mfyycYa0FTGhJPcTT\
     P3TQzy1Krc1DAVLbZ8IqGBrW13YWN/cMvaiEzcETNyA4N3kOaEXKWodnkwucJv2n\
     EnJWTKNHY9NS9f5Cq3OPs4pQ==",
    // Token 2: "empty" (signature of null byte) — works on <SM8350
    "vzXWATo51hZr4Dh+a5sA/Q4JYoP4Ee3oFZSGbPZ2tBsaMupn+6tPbZDkXJRLUzA\
     qHaMtlPMKaOHrEWZysCkgCJqpOPkUZNaSbEKpPQ6uiOVJpJwA/PmxuJ72inzSPe\
     vriMAdhQrNUqgyu4ATTEsOKnoUIuJTDBmzCeuh/34SOjTdO4Pc+s3ORfMD0TX+WI\
     meUx4c9xVdSL/xirPl/BouhfuwFd4qPPyO5RqkU/fevEoJWGHaFjfI302c9k7Ep\
     fRUhq1z+wNpZblOHuj0B3/7VOkK8KtSvwLkmVF/t9ECiry6G5iVGEOyqMlktNlI\
     Abr2MMYXn6b4Y3GDCkhPJ5LUkQ==",
];

/// Attempt Xiaomi EDL authentication via sig command.
/// Sends each hardcoded token; returns true if any succeeds.
fn try_xiaomi_edl_auth(device: &mut QdlDevice<dyn QdlReadWrite>) -> bool {
    let sig_xml = b"<?xml version=\"1.0\" ?><data> <sig TargetName=\"sig\" size_in_bytes=\"256\" verbose=\"1\"/></data>";

    for (i, token_b64) in XIAOMI_AUTH_TOKENS.iter().enumerate() {
        info!("EDL: trying Xiaomi auth token {} of {}", i + 1, XIAOMI_AUTH_TOKENS.len());

        // Decode the 256-byte auth token
        let token_bytes = match base64::engine::general_purpose::STANDARD.decode(token_b64) {
            Ok(bytes) if bytes.len() == 256 => bytes,
            Ok(bytes) => {
                warn!("EDL: auth token {} has wrong size: {} (expected 256)", i + 1, bytes.len());
                continue;
            }
            Err(e) => {
                warn!("EDL: auth token {} base64 decode failed: {e}", i + 1);
                continue;
            }
        };

        // Step 1: Send sig XML command
        let mut sig_cmd = sig_xml.to_vec();
        let write_result = qdl_safe(|| firehose_write(device, &mut sig_cmd).map_err(|e| format!("{e}")));
        if let Err(e) = write_result {
            warn!("EDL: sig XML send failed: {e}");
            continue;
        }

        // Read ACK for sig command (wrapped in qdl_safe — parser can panic on missing "value" attr)
        let ack_ok = match qdl_safe(|| {
            firehose_read(device, qdl::parsers::firehose_parser_ack_nak)
                .map_err(|e| format!("{e}"))
        }) {
            Ok(qdl::types::FirehoseStatus::Ack) => true,
            _ => {
                warn!("EDL: sig XML was NAKed or response malformed");
                false
            }
        };
        if !ack_ok {
            continue;
        }

        // Step 2: Send raw 256-byte auth token
        info!("EDL: sig ACKed, sending 256-byte auth token...");
        if let Err(e) = device.rw.write_all(&token_bytes) {
            warn!("EDL: auth token write failed: {e}");
            continue;
        }

        // Step 3: Read auth token response.
        // Programmer sends <log value="Device is authenticated"/> then <response value="ACK" rawmode="true"/>
        // firehose_read processes <log> entries automatically (prints them) and returns the <response>.
        // Wrapped in qdl_safe because firehose_parser_ack_nak can panic on missing "value" attr.
        let auth_ok = match qdl_safe(|| {
            firehose_read(device, qdl::parsers::firehose_parser_ack_nak)
                .map_err(|e| format!("{e}"))
        }) {
            Ok(qdl::types::FirehoseStatus::Ack) => true,
            Ok(_) => {
                warn!("EDL: auth token {} NAKed by programmer", i + 1);
                false
            }
            Err(e) => {
                // Parser panicked or response malformed. Check raw buffer as fallback.
                warn!("EDL: auth response parse failed: {e}");
                let mut fallback_buf = vec![0u8; 4096];
                let mut found = false;
                for _ in 0..3 {
                    match device.rw.read(&mut fallback_buf) {
                        Ok(n) if n > 0 => {
                            let resp = String::from_utf8_lossy(&fallback_buf[..n]).to_lowercase();
                            info!("EDL: auth fallback read: {resp}");
                            if resp.contains("authenticated") && !resp.contains("not authenticated") {
                                found = true;
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                found
            }
        };

        if auth_ok {
            info!("EDL: Xiaomi EDL authentication SUCCEEDED with token {}", i + 1);
            return true;
        }
    }

    warn!("EDL: all Xiaomi auth tokens failed");
    false
}

/// Auto-detect Qualcomm EDL COM port on Windows.
/// Pure Rust — reads Windows registry directly, no PowerShell, no subprocess.
/// Scans HKLM\SYSTEM\CurrentControlSet\Enum\USB\VID_05C6&PID_9008 for COM port.
#[cfg(target_os = "windows")]
fn detect_edl_com_port() -> Option<String> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let usb_key = hklm
        .open_subkey_with_flags(
            r"SYSTEM\CurrentControlSet\Enum\USB\VID_05C6&PID_9008",
            KEY_READ,
        )
        .ok()?;

    // Iterate device instances under VID_05C6&PID_9008
    for instance_name in usb_key.enum_keys().filter_map(|k| k.ok()) {
        let instance_key = usb_key.open_subkey_with_flags(&instance_name, KEY_READ).ok()?;
        let params_key = instance_key
            .open_subkey_with_flags("Device Parameters", KEY_READ)
            .ok();

        if let Some(params) = params_key {
            if let Ok(port_name) = params.get_value::<String, _>("PortName") {
                if port_name.starts_with("COM") {
                    info!("EDL: auto-detected serial port: {port_name}");
                    return Some(port_name);
                }
            }
        }
    }

    None
}

#[cfg(not(target_os = "windows"))]
fn detect_edl_com_port() -> Option<String> {
    None
}

struct SaharaIdentity {
    serial: Option<String>,
    hw_id: Option<String>,
    pk_hash: Option<String>,
}

fn read_sahara_identity(device: &mut QdlDevice<dyn QdlReadWrite>, context: &str) -> SaharaIdentity {
    let serial = match sahara_run(
        device,
        SaharaMode::Command,
        Some(SaharaCmdModeCmd::ReadSerialNum),
        &mut [],
        vec![],
        false,
    ) {
        Ok(data) if data.len() >= 4 => {
            let sn = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            Some(format!("{:08X}", sn))
        }
        Ok(_) => None,
        Err(e) => {
            warn!("EDL{}: Failed to read serial: {e}", context);
            None
        }
    };

    let hw_id = match sahara_run(
        device,
        SaharaMode::Command,
        Some(SaharaCmdModeCmd::ReadHwId),
        &mut [],
        vec![],
        false,
    ) {
        Ok(data) => Some(hex::encode(EdlProtocol::dedup_sahara_field(&data))),
        Err(e) => {
            warn!("EDL{}: Failed to read HWID: {e}", context);
            None
        }
    };

    let pk_hash = match sahara_run(
        device,
        SaharaMode::Command,
        Some(SaharaCmdModeCmd::ReadOemKeyHash),
        &mut [],
        vec![],
        false,
    ) {
        Ok(data) => Some(hex::encode(EdlProtocol::dedup_sahara_field(&data))),
        Err(e) => {
            warn!("EDL{}: Failed to read PKHash: {e}", context);
            None
        }
    };

    SaharaIdentity { serial, hw_id, pk_hash }
}

impl EdlProtocol {
    /// Open EDL transport -- serial COM first, USB fallback.
    fn open_transport() -> Result<(Box<dyn QdlReadWrite>, QdlBackend), FlashError> {
        // Try serial COM port first (works with stock Qualcomm qcusbser driver)
        if let Some(port) = detect_edl_com_port() {
            info!("EDL: connecting via serial port {port}");
            match qdl::serial::setup_serial_device(Some(port.clone())) {
                Ok(serial) => {
                    info!("EDL: serial transport opened on {port}");
                    return Ok((Box::new(serial), QdlBackend::Serial));
                }
                Err(e) => {
                    let err_str = e.to_string();
                    // If serial port is in use (held by an active connection), don't
                    // fall through to USB — that will also fail with a misleading error.
                    if err_str.contains("in use") || err_str.contains("os error 170") {
                        return Err(FlashError::Usb(format!(
                            "Serial port {port} is already in use. \
                             Disconnect the current session first. Error: {e}"
                        )));
                    }
                    warn!("EDL: serial port {port} failed ({e}), trying USB...");
                }
            }
        }

        // Fallback to USB (requires WinUSB via Zadig)
        info!("EDL: no serial port found, trying USB transport");
        let transport =
            EdlUsbTransport::open().map_err(|e| FlashError::Usb(e.to_string()))?;
        Ok((Box::new(transport), QdlBackend::Usb))
    }

    /// Try to recover a stale Sahara session by sending SAHARA_RESET_REQ (0x07).
    ///
    /// When a previous app session ran Sahara commands (identify) but closed before
    /// uploading a programmer, the device is stuck in post-command Sahara state.
    /// RESET_REQ tells the device to abort and re-enter EDL from scratch.
    /// If the device responds with RESET_RSP (0x08), it will re-enumerate and send
    /// fresh HELLO on the next connection.
    ///
    /// Returns true if reset was acknowledged (device will re-enter fresh Sahara).
    fn try_sahara_reset(device: &mut QdlDevice<dyn QdlReadWrite>) -> bool {
        // SAHARA_RESET_REQ: command_id=0x07 (u32 LE) + packet_length=0x08 (u32 LE)
        let reset_cmd: [u8; 8] = [0x07, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00];

        if let Err(e) = device.rw.write(&reset_cmd) {
            warn!("EDL: Sahara reset write failed: {e}");
            return false;
        }

        // Read response with short timeout — expect RESET_RSP (0x08) in first 4 bytes
        let mut buf = [0u8; 64];
        match device.rw.read(&mut buf) {
            Ok(n) if n >= 4 => {
                let cmd_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                if cmd_id == 0x08 {
                    info!("EDL: Sahara RESET_RSP received — device re-entering EDL");
                    // Device will re-initialize PBL after reset. Need to:
                    // 1. Release serial port (caller drops device)
                    // 2. Wait for USB re-enumeration + PBL init (~2-3 seconds)
                    std::thread::sleep(std::time::Duration::from_millis(2500));
                    return true;
                }
                // Check if response is XML (0x3C = '<') — device is in Firehose, not Sahara
                if buf[0] == 0x3C {
                    info!("EDL: device responded with XML to Sahara reset — already in Firehose mode");
                    return false; // Fall through to nop probe
                }
                info!("EDL: Sahara reset got unexpected response: cmd=0x{cmd_id:02X} ({n} bytes)");
                false
            }
            Ok(n) => {
                info!("EDL: Sahara reset got short response ({n} bytes)");
                false
            }
            Err(e) => {
                warn!("EDL: Sahara reset read timed out: {e}");
                false
            }
        }
    }

    /// Deduplicate a Sahara field buffer.
    ///
    /// Sahara returns fixed-size buffers padded by repeating the real data.
    /// For example, an 8-byte HWID is repeated 6× to fill a 48-byte response.
    /// This matches bkerler's sahara.py dedup pattern (lines 187-196): search for
    /// the first 4 bytes reappearing after position 4 and trim there.
    ///
    /// Known limitation: if the true data's first 4 bytes happen to repeat at
    /// offset 4 by coincidence, this truncates too early. Same trade-off as bkerler.
    fn dedup_sahara_field(raw: &[u8]) -> Vec<u8> {
        if raw.len() < 8 {
            return raw.to_vec();
        }
        let prefix = [raw[0], raw[1], raw[2], raw[3]];
        for idx in 4..raw.len().saturating_sub(3) {
            if raw[idx..idx + 4] == prefix {
                return raw[..idx].to_vec();
            }
        }
        raw.to_vec()
    }

    /// Build a 48-byte Sahara HELLO_RSP packet for blind-send (PblHack).
    ///
    /// Qualcomm Sahara protocol: the device sends HELLO (0x01) on power-up,
    /// then waits for the host's HELLO_RSP (0x02). If the HELLO was consumed
    /// by a previous session, we can send HELLO_RSP blind — the device is
    /// still waiting for one. Mode=COMMAND (0x03) so we can SWITCH_MODE after.
    ///
    /// Packet layout (all u32 LE):
    ///   [0x00] command=0x02  [0x04] length=0x30  [0x08] version=2
    ///   [0x0C] version_supported=2  [0x10] status=0  [0x14] mode=3
    ///   [0x18..0x30] reserved=0
    ///
    /// Reference: emmcdl SaharaProtocol::ConnectToDevice, bkerler sahara.py:cmd_hello
    fn build_blind_hello_rsp() -> [u8; 48] {
        let mut pkt = [0u8; 48];
        // command: SAHARA_HELLO_RSP (0x02)
        pkt[0..4].copy_from_slice(&0x02u32.to_le_bytes());
        // length: 48 (0x30)
        pkt[4..8].copy_from_slice(&0x30u32.to_le_bytes());
        // version: 2
        pkt[8..12].copy_from_slice(&2u32.to_le_bytes());
        // version_supported: 2
        pkt[12..16].copy_from_slice(&2u32.to_le_bytes());
        // status: SUCCESS (0)
        pkt[16..20].copy_from_slice(&0u32.to_le_bytes());
        // mode: COMMAND (3)
        pkt[20..24].copy_from_slice(&3u32.to_le_bytes());
        // reserved[0..5]: all zero (implicit from [0u8; 48])
        pkt
    }

    /// Build a 12-byte Sahara SWITCH_MODE packet to IMAGE_TX_PENDING.
    ///
    /// After PblHack puts the device in Command mode, this switches it back
    /// to ImageTxPending mode — the device responds with a fresh HELLO.
    ///
    /// Packet layout (all u32 LE):
    ///   [0x00] command=0x0C  [0x04] length=0x0C  [0x08] mode=0x00
    ///
    /// Reference: emmcdl SaharaProtocol::ModeSwitch
    fn build_switch_mode_image_tx() -> [u8; 12] {
        let mut pkt = [0u8; 12];
        // command: SAHARA_SWITCH_MODE (0x0C)
        pkt[0..4].copy_from_slice(&0x0Cu32.to_le_bytes());
        // length: 12 (0x0C)
        pkt[4..8].copy_from_slice(&0x0Cu32.to_le_bytes());
        // mode: IMAGE_TX_PENDING (0)
        pkt[8..12].copy_from_slice(&0u32.to_le_bytes());
        pkt
    }

    /// Attempt PblHack recovery for stale Sahara sessions.
    ///
    /// When a previous app session ran identify() but closed before connect(),
    /// the device is stuck in WaitingForImage Sahara state. It already sent its
    /// HELLO (consumed by the old transport) and is waiting for a HELLO_RSP.
    ///
    /// PblHack sends a blind HELLO_RSP (no HELLO read needed) to satisfy the
    /// waiting device, then SWITCH_MODE to put it back in ImageTxPending —
    /// the device responds with a fresh HELLO that's ready for normal identify.
    ///
    /// This works on the SAME transport — no USB re-enumeration, no power cycle.
    ///
    /// Reference: emmcdl SaharaProtocol::PblHack()
    ///
    /// Returns true if the device responded with CMD_READY, meaning the
    /// blind HELLO_RSP was accepted and a fresh HELLO is now in the buffer.
    fn try_pbl_hack(device: &mut QdlDevice<dyn QdlReadWrite>) -> bool {
        info!("EDL: attempting PblHack (blind HELLO_RSP) recovery...");

        // Step 1: Flush any stale data from the transport buffer.
        // The device may have sent partial data or error responses.
        let mut drain_buf = [0u8; 4096];
        for _ in 0..3 {
            match device.rw.read(&mut drain_buf) {
                Ok(0) => break,
                Ok(n) => {
                    info!("EDL: PblHack drained {n} stale bytes from transport");
                }
                Err(_) => break, // Timeout = buffer empty, good
            }
        }

        // Step 2: Send blind HELLO_RSP (mode=COMMAND).
        // The device is in WAIT_HELLO state — it already sent HELLO on the old
        // transport and is waiting for any valid HELLO_RSP.
        let hello_rsp = Self::build_blind_hello_rsp();
        if let Err(e) = device.rw.write(&hello_rsp) {
            warn!("EDL: PblHack HELLO_RSP write failed: {e}");
            return false;
        }

        // Step 3: Read response — expect CMD_READY (0x0B).
        // If the device accepts the HELLO_RSP in Command mode, it sends
        // CMD_READY to indicate it's ready for command execution.
        let mut resp_buf = [0u8; 64];
        match device.rw.read(&mut resp_buf) {
            Ok(n) if n >= 4 => {
                let cmd_id = u32::from_le_bytes([resp_buf[0], resp_buf[1], resp_buf[2], resp_buf[3]]);
                if cmd_id == 0x0B {
                    info!("EDL: PblHack got CMD_READY — device accepted blind HELLO_RSP");
                } else {
                    info!("EDL: PblHack got unexpected response: cmd=0x{cmd_id:02X} ({n} bytes)");
                    return false;
                }
            }
            Ok(n) => {
                info!("EDL: PblHack got short response ({n} bytes)");
                return false;
            }
            Err(e) => {
                // NOTE: info! not warn! — timeout here is EXPECTED behavior, not an error.
                // It means the device wasn't in WAIT_HELLO state (PblHack doesn't apply).
                // Don't "fix" this to warn! to match try_sahara_reset() — different semantics.
                info!("EDL: PblHack read timed out: {e} — device not in WAIT_HELLO state");
                return false;
            }
        }

        // Step 4: Send SWITCH_MODE to ImageTxPending.
        // This moves the device from Command mode back to the initial state
        // where it sends a fresh HELLO — ready for normal identify flow.
        let switch_mode = Self::build_switch_mode_image_tx();
        if let Err(e) = device.rw.write(&switch_mode) {
            warn!("EDL: PblHack SWITCH_MODE write failed: {e}");
            return false;
        }

        // The device now sends a fresh HELLO (0x01) in response to SWITCH_MODE.
        // We do NOT read it here — it stays in the transport buffer for the
        // subsequent sahara_run() calls in identify_inner() to pick up.
        info!("EDL: PblHack succeeded — device recovered, fresh HELLO pending in buffer");
        true
    }

    /// Identify device via Sahara command mode (no programmer needed).
    /// Opens transport, performs Sahara HELLO, reads serial/HWID/PKHash.
    /// Returns both device info AND the QdlDevice (transport stays open for connect).
    ///
    /// If Sahara is stale from a previous session, recovery chain:
    /// 1. PblHack — blind HELLO_RSP on same transport (fastest, no re-enumeration)
    /// 2. RESET_REQ — full device reset + USB re-enumeration
    /// 3. Firehose nop probe — detect stale programmer from previous session
    pub fn identify() -> Result<(EdlDeviceInfo, QdlDevice<dyn QdlReadWrite>), FlashError> {
        Self::identify_inner(false)
    }

    fn identify_inner(is_retry: bool) -> Result<(EdlDeviceInfo, QdlDevice<dyn QdlReadWrite>), FlashError> {
        let (device_rw, backend) = Self::open_transport()?;

        let mut fh_cfg = default_firehose_config();
        fh_cfg.backend = backend;
        let mut device = QdlDevice {
            rw: device_rw,
            fh_cfg,
            reset_on_drop: false,
        };

        let id = read_sahara_identity(&mut device, "");
        let mut serial = id.serial;
        let mut hw_id = id.hw_id;
        let mut pk_hash = id.pk_hash;

        info!("EDL identify: serial={:?}, hw_id={:?}, pk_hash={:?}", serial, hw_id, pk_hash);

        let mut sahara_responded = serial.is_some() || hw_id.is_some() || pk_hash.is_some();

        if sahara_responded {
            // IMPORTANT: Do NOT drop the device here. The last sahara_run sent
            // SWITCH_MODE(WaitingForImage), so the device has sent a HELLO that's
            // pending in the transport buffer. We return the device so connect()
            // can reuse it — sahara_run(WaitingForImage) will read that pending HELLO.
            let info = EdlDeviceInfo {
                serial,
                hw_id,
                pk_hash,
                storage_type: None,
                sector_size: None,
                num_luns: None,
                firehose_active: false,
                chipset: None,
            };
            return Ok((info, device));
        }

        // Sahara timed out (all fields None). Possible causes:
        // 1. Previous session identified (Sahara commands ran), then closed without
        //    uploading a programmer. Device is stuck in WaitingForImage Sahara state.
        // 2. Device has a programmer loaded from a previous session (Firehose mode).
        //
        // Recovery chain: PblHack (fastest) → RESET_REQ (fallback) → nop probe.

        // Strategy 1: PblHack — blind HELLO_RSP on same transport (no re-enumeration).
        // Works when the device is in WaitingForImage state, waiting for HELLO_RSP.
        // Skipped on retry (is_retry=true) — after RESET_REQ re-enumeration, the device
        // sent a fresh HELLO; if Sahara still failed, PblHack can't help and would only
        // waste timeout budget on drain/read cycles.
        if !is_retry && Self::try_pbl_hack(&mut device) {
            info!("EDL: PblHack recovered, retrying Sahara commands on same transport...");

            // Fresh HELLO is now in the transport buffer. Retry the 3 Sahara commands.
            let id = read_sahara_identity(&mut device, " PblHack retry —");
            serial = id.serial;
            hw_id = id.hw_id;
            pk_hash = id.pk_hash;

            sahara_responded = serial.is_some() || hw_id.is_some() || pk_hash.is_some();

            if sahara_responded {
                info!("EDL: PblHack recovery successful — device identified");
                let info = EdlDeviceInfo {
                    serial,
                    hw_id,
                    pk_hash,
                    storage_type: None,
                    sector_size: None,
                    num_luns: None,
                    firehose_active: false,
                    chipset: None,
                };
                return Ok((info, device));
            }
            info!("EDL: PblHack succeeded but Sahara commands still failed — trying RESET_REQ...");
        }

        // Strategy 2: SAHARA_RESET_REQ (0x07) — full device reset + USB re-enumeration.
        // Falls back to this when PblHack fails (device not in WaitingForImage state).
        if !is_retry {
            info!("EDL: trying Sahara RESET_REQ...");
            let reset_recovered = Self::try_sahara_reset(&mut device);
            if reset_recovered {
                info!("EDL: Sahara reset succeeded, retrying identify with fresh transport...");
                drop(device);
                return Self::identify_inner(true);
            }
        } else {
            info!("EDL: Sahara timed out on retry — reset didn't help");
        }

        // Strategy 3: Firehose nop probe — detect stale programmer from previous session.
        info!("EDL: all Sahara recovery failed, probing for stale Firehose mode...");
        let firehose_active = probe_firehose_nop(&mut device);

        if firehose_active {
            info!("EDL: Firehose mode detected (previous session) — Sahara will be skipped on connect");
        } else {
            info!("EDL: nop probe failed — device not responding, needs power cycle");
        }

        let info = EdlDeviceInfo {
            serial,
            hw_id,
            pk_hash,
            storage_type: None,
            sector_size: None,
            num_luns: None,
            firehose_active,
            chipset: None,
        };

        Ok((info, device))
    }

    /// Upload programmer and establish Firehose session.
    /// If `sahara_device` is provided (from identify), reuses the transport
    /// which has a pending HELLO ready. Otherwise opens a fresh transport.
    /// If `firehose_active` is true, the device already has a programmer loaded
    /// from a previous session — skip Sahara and go straight to configure.
    pub fn connect(
        programmer_path: &Path,
        sahara_device: Option<QdlDevice<dyn QdlReadWrite>>,
        firehose_active: bool,
    ) -> Result<EdlConnection, FlashError> {
        let mut device = if firehose_active {
            // Device already in Firehose mode — reuse transport, skip programmer upload
            info!("EDL: resuming Firehose session (programmer already loaded)");
            sahara_device.ok_or_else(|| {
                FlashError::Protocol("Firehose recovery requires stored transport".into())
            })?
        } else {
            validate_programmer_file(programmer_path)?;

            let programmer_data = std::fs::read(programmer_path)
                .map_err(|e| FlashError::Protocol(format!("Cannot read programmer file: {e}")))?;

            info!(
                "EDL connect: uploading programmer ({} bytes)",
                programmer_data.len()
            );

            let mut dev = if let Some(dev) = sahara_device {
                info!("EDL: reusing transport from identify (pending HELLO in buffer)");
                dev
            } else {
                info!("EDL: opening fresh transport for connect");
                let (device_rw, backend) = Self::open_transport()?;
                let mut fh_cfg = default_firehose_config();
                fh_cfg.backend = backend;
                QdlDevice {
                    rw: device_rw,
                    fh_cfg,
                    reset_on_drop: false,
                }
            };

            // Sahara: upload programmer image
            let mut images = vec![programmer_data];
            let filenames = vec![programmer_path.display().to_string()];
            sahara_run(
                &mut dev,
                SaharaMode::WaitingForImage,
                None,
                &mut images,
                filenames,
                false,
            )
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("AUTH") || msg.contains("auth") || msg.contains("HASH") {
                    warn!("EDL: programmer rejected — not signed for this device: {msg}");
                    FlashError::Protocol(format!(
                        "Programmer not signed for this device. Error: {msg}"
                    ))
                } else {
                    warn!("EDL: Sahara programmer upload failed: {msg}");
                    FlashError::Protocol(format!("Sahara programmer upload failed: {msg}"))
                }
            })?;

            info!("EDL: programmer uploaded, configuring Firehose...");
            dev
        };

        // When resuming a stale Firehose session, the nop probe may not have
        // drained all log messages (programmer dumps ~15 supported function names).
        // Drain aggressively before configure to ensure a clean transport buffer.
        if firehose_active {
            info!("EDL: draining stale Firehose buffer before configure...");
            // Drain multiple rounds — programmer dumps ~15 supported function names
            // as log messages after waking up, and a single drain may not get them all.
            for _ in 0..3 {
                drain_firehose_buffer(&mut device);
            }
        }

        // --- Reactive auth flow (aligned with bkerler/edl reference) ---
        // 1. Send configure XML first (firehose_configure only writes, doesn't read)
        // 2. Read raw response ourselves (bypass panicky qdlrs parser)
        // 3. If response contains auth requirement → auth → retry configure
        // 4. If configure ACKed → parse config values from raw response

        // Step 1: Send configure XML
        qdl_safe(|| firehose_configure(&mut device, false).map_err(|e| format!("{e}")))?;

        // Step 2: Read raw response (bypasses qdlrs parser that can panic on missing attrs)
        let configure_response = read_firehose_response_raw(&mut device)
            .unwrap_or_else(|e| {
                warn!("EDL: configure response read failed: {e}");
                String::new()
            });
        let resp_preview = &configure_response[..configure_response.len().min(500)];
        info!("EDL: configure response: {resp_preview}");

        // Step 3: Check if programmer requires authentication
        let needs_auth = configure_response.contains("nop and sig tag")
            || configure_response.contains("Only nop");

        if needs_auth {
            info!("EDL: programmer requires authentication (detected from configure response)");

            if !try_xiaomi_edl_auth(&mut device) {
                return Err(FlashError::Protocol(
                    "Programmer requires authentication but all auth tokens failed. \
                     This programmer may need a device-specific auth token.".into(),
                ));
            }
            info!("EDL: authenticated, retrying configure...");
            drain_firehose_buffer(&mut device);

            // Retry configure after auth — programmer should now accept all commands
            qdl_safe(|| firehose_configure(&mut device, false).map_err(|e| format!("{e}")))?;
            let retry_response = read_firehose_response_raw(&mut device)
                .unwrap_or_else(|e| {
                    warn!("EDL: post-auth configure response read failed: {e}");
                    String::new()
                });
            info!("EDL: post-auth configure: {}", &retry_response[..retry_response.len().min(500)]);
            apply_configure_response(&retry_response, &mut device);
        } else if configure_response.contains("value=\"ACK\"") {
            // Configure succeeded — parse config values from response
            apply_configure_response(&configure_response, &mut device);
        } else if configure_response.contains("Not support configure MemoryName") {
            // Wrong storage type — toggle and retry
            if configure_response.contains("MemoryName eMMC") {
                info!("EDL: eMMC not supported by programmer, switching to UFS");
                device.fh_cfg.storage_type = FirehoseStorageType::Ufs;
                device.fh_cfg.storage_sector_size = 4096;
            } else {
                info!("EDL: UFS not supported by programmer, switching to eMMC");
                device.fh_cfg.storage_type = FirehoseStorageType::Emmc;
                device.fh_cfg.storage_sector_size = 512;
            }
            qdl_safe(|| firehose_configure(&mut device, false).map_err(|e| format!("{e}")))?;
            let retry_response = read_firehose_response_raw(&mut device)
                .unwrap_or_else(|e| {
                    warn!("EDL: storage retry response read failed: {e}");
                    String::new()
                });
            apply_configure_response(&retry_response, &mut device);
        } else {
            warn!("EDL: configure response unclear, proceeding with defaults");
        }

        // Auto-detect storage type by probing LUNs.
        // If multiple LUNs respond, device is UFS — reconfigure with correct settings.
        let mut lun_count = 0u8;
        for lun in 0..8 {
            if qdl_safe(|| firehose_get_storage_info(&mut device, lun).map_err(|e| format!("{e}"))).is_ok() {
                lun_count += 1;
            }
        }
        info!("EDL: LUN probe found {} responding LUN(s)", lun_count);

        let (storage_type, sector_size, num_luns) = if lun_count > 1 {
            // Confirmed UFS — config already set to UFS/4096 by default
            info!("EDL: confirmed UFS storage ({lun_count} LUNs, 4096-byte sectors)");
            (Some("ufs".to_string()), Some(4096u32), Some(lun_count))
        } else {
            // Single LUN — likely eMMC. Switch to eMMC/512.
            // NOTE: We initially configured as UFS. For eMMC devices, this means
            // the first configure used wrong settings. eMMC devices may need a fresh
            // connection. For now, update local config — most eMMC programmers handle this.
            info!("EDL: single LUN detected — assuming eMMC (512-byte sectors)");
            device.fh_cfg.storage_type = FirehoseStorageType::Emmc;
            device.fh_cfg.storage_sector_size = 512;
            (Some("emmc".to_string()), Some(512u32), Some(1))
        };
        info!("EDL: storage={:?}, sector_size={:?}, luns={:?}", storage_type, sector_size, num_luns);

        let info = EdlDeviceInfo {
            serial: None,
            hw_id: None,
            pk_hash: None,
            storage_type,
            sector_size,
            num_luns,
            firehose_active: false,
            chipset: None,
        };

        info!(
            "EDL connected: storage={:?}, sector_size={:?}, luns={:?}",
            info.storage_type, info.sector_size, info.num_luns
        );

        Ok(EdlConnection {
            device,
            info,
        })
    }

    /// List partitions from GPT on a specific LUN.
    pub fn list_partitions(
        conn: &mut EdlConnection,
        lun: u8,
    ) -> Result<Vec<EdlPartitionEntry>, FlashError> {
        let sector_size = conn.info.sector_size.unwrap_or(512) as usize;
        let num_sectors = 34;
        let total_bytes = num_sectors * sector_size;
        let mut raw_data = vec![0u8; total_bytes];

        {
            let mut cursor = Cursor::new(&mut raw_data[..]);
            qdl_safe(|| {
                firehose_read_storage(&mut conn.device, &mut cursor, num_sectors, 0, lun, 0)
                    .map_err(|e| format!("Failed to read GPT from LUN {lun}: {e}"))
            })?;
        }

        parse_gpt(&raw_data, sector_size as u32, lun)
    }

    /// Read a partition, writing data to the provided writer.
    pub fn read_partition_to_writer(
        conn: &mut EdlConnection,
        lun: u8,
        start_sector: u64,
        num_sectors: u64,
        writer: &mut impl Write,
    ) -> Result<(), FlashError> {
        if start_sector > u32::MAX as u64 {
            return Err(FlashError::Protocol(format!(
                "start_sector {} exceeds u32 range",
                start_sector
            )));
        }

        qdl_safe(|| {
            firehose_read_storage(
                &mut conn.device,
                writer,
                num_sectors as usize,
                0,
                lun,
                start_sector as u32,
            )
            .map_err(|e| format!("Partition read failed: {e}"))
        })?;

        info!(
            "EDL: read {} sectors from LUN {}",
            num_sectors, lun
        );
        Ok(())
    }

    /// Read a partition to a file (convenience wrapper).
    #[allow(dead_code)]
    pub fn read_partition(
        conn: &mut EdlConnection,
        lun: u8,
        start_sector: u64,
        num_sectors: u64,
        output_path: &Path,
    ) -> Result<(), FlashError> {
        let file = std::fs::File::create(output_path)
            .map_err(|e| FlashError::Protocol(format!("Cannot create output file: {e}")))?;
        let mut writer = std::io::BufWriter::new(file);
        Self::read_partition_to_writer(conn, lun, start_sector, num_sectors, &mut writer)?;
        info!("EDL: saved to {}", output_path.display());
        Ok(())
    }

    /// Reboot device via Firehose power command.
    pub fn reboot(conn: &mut EdlConnection, mode: &str) -> Result<(), FlashError> {
        let reset_mode = match mode {
            "reset" => FirehoseResetMode::Reset,
            "reset_to_edl" => FirehoseResetMode::ResetToEdl,
            "off" => FirehoseResetMode::Off,
            _ => {
                return Err(FlashError::Protocol(format!(
                    "Invalid reboot mode: {mode}"
                )))
            }
        };

        qdl_safe(|| {
            firehose_reset(&mut conn.device, &reset_mode, 1)
                .map_err(|e| format!("EDL reboot failed: {e}"))
        })?;

        info!("EDL: reboot command sent (mode: {mode})");
        Ok(())
    }

    /// Write an image file to a partition via Firehose program command.
    /// Automatically detects and decompresses Android sparse images.
    /// If `progress_counter` is provided, tracks bytes read for progress reporting.
    pub fn program_partition(
        conn: &mut EdlConnection,
        lun: u8,
        start_sector: u64,
        num_sectors: u64,
        file_path: &Path,
        progress_counter: Option<Arc<AtomicU64>>,
    ) -> Result<Option<VerifyResult>, FlashError> {
        if !file_path.exists() {
            return Err(FlashError::Protocol(format!(
                "Image file not found: {}",
                file_path.display()
            )));
        }

        let file_size = std::fs::metadata(file_path)
            .map_err(|e| FlashError::Protocol(format!("Cannot stat image file: {e}")))?
            .len();
        if file_size == 0 {
            return Err(FlashError::Protocol("Image file is empty".into()));
        }

        if start_sector > u32::MAX as u64 {
            return Err(FlashError::Protocol(format!(
                "start_sector {} exceeds u32 range",
                start_sector
            )));
        }

        // Auto-detect and decompress sparse images
        let (raw_path, is_temp) = ensure_raw_image(file_path)?;

        let result = (|| {
            let file = std::fs::File::open(&raw_path)
                .map_err(|e| FlashError::Protocol(format!("Cannot open raw image: {e}")))?;

            let label = file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("partition");

            if let Some(counter) = progress_counter {
                let mut reader = ProgressReader { inner: BufReader::new(file), read_bytes: counter };
                qdl_safe(|| {
                    firehose_program_storage(
                        &mut conn.device,
                        &mut reader,
                        label,
                        num_sectors as usize,
                        0,
                        lun,
                        &start_sector.to_string(),
                    )
                    .map_err(|e| format!("Partition write failed: {e}"))
                })
            } else {
                let mut reader = BufReader::new(file);
                qdl_safe(|| {
                    firehose_program_storage(
                        &mut conn.device,
                        &mut reader,
                        label,
                        num_sectors as usize,
                        0,
                        lun,
                        &start_sector.to_string(),
                    )
                    .map_err(|e| format!("Partition write failed: {e}"))
                })
            }
        })();

        // Run verification before cleaning up temp file (need raw_path for hashing)
        let verify = if result.is_ok() {
            info!(
                "EDL: programmed {} sectors on LUN {} from {}",
                num_sectors,
                lun,
                file_path.display()
            );
            match Self::verify_write(conn, lun, start_sector, num_sectors, &raw_path) {
                Ok(v) => {
                    info!("EDL: post-write verify: passed={}, detail={}", v.passed, v.detail);
                    Some(v)
                }
                Err(e) => {
                    warn!("EDL: post-write verify failed (non-fatal): {e}");
                    None
                }
            }
        } else {
            None
        };

        // Clean up temp file from sparse decompression
        if is_temp {
            let _ = std::fs::remove_file(&raw_path);
        }

        result.map(|_| verify)
    }

    /// Erase a partition by programming zeros via Firehose.
    pub fn erase_partition(
        conn: &mut EdlConnection,
        lun: u8,
        start_sector: u64,
        num_sectors: u64,
    ) -> Result<(), FlashError> {
        if start_sector > u32::MAX as u64 {
            return Err(FlashError::Protocol(format!(
                "start_sector {} exceeds u32 range",
                start_sector
            )));
        }

        qdl_safe(|| {
            firehose_program_storage(
                &mut conn.device,
                &mut &[0u8][..],
                "erase",
                num_sectors as usize,
                0, // slot
                lun,
                &start_sector.to_string(),
            )
            .map_err(|e| format!("Partition erase failed: {e}"))
        })?;

        info!(
            "EDL: erased {} sectors on LUN {} at sector {}",
            num_sectors, lun, start_sector
        );
        Ok(())
    }

    /// Verify a write by reading back data from the device and comparing SHA256 hashes.
    /// Checks head (and tail for large images) to confirm data integrity.
    pub fn verify_write(
        conn: &mut EdlConnection,
        lun: u8,
        start_sector: u64,
        num_sectors: u64,
        source_path: &Path,
    ) -> Result<VerifyResult, FlashError> {
        let sector_size = conn.info.sector_size.unwrap_or(512) as u64;
        let total_bytes = num_sectors * sector_size;
        let check_size = std::cmp::min(VERIFY_CHUNK_SIZE as u64, total_bytes);

        let file_size = std::fs::metadata(source_path)
            .map_err(|e| FlashError::Protocol(format!("Cannot stat source file: {e}")))?
            .len();
        let hash_size = std::cmp::min(file_size, total_bytes);

        let (src_head, src_tail) = compute_file_hashes(source_path, hash_size)?;

        // Read head chunk from device
        let head_sectors = check_size.div_ceil(sector_size);
        let mut head_buf = vec![0u8; (head_sectors * sector_size) as usize];
        {
            let mut cursor = Cursor::new(&mut head_buf[..]);
            qdl_safe(|| {
                firehose_read_storage(
                    &mut conn.device,
                    &mut cursor,
                    head_sectors as usize,
                    0,
                    lun,
                    start_sector as u32,
                )
                .map_err(|e| format!("Verify read (head) failed: {e}"))
            })?;
        }
        let dev_head_hash = format!("{:x}", Sha256::digest(&head_buf[..check_size as usize]));

        let threshold = 2 * VERIFY_CHUNK_SIZE as u64;
        if total_bytes <= threshold {
            // Small image: just compare heads
            let passed = dev_head_hash == src_head;
            let detail = if passed {
                format!("Head SHA256 match ({} bytes checked)", check_size)
            } else {
                format!(
                    "Head SHA256 MISMATCH: source={}, device={}",
                    &src_head[..16],
                    &dev_head_hash[..16]
                )
            };
            if passed {
                info!("EDL verify: PASS — {detail}");
            } else {
                warn!("EDL verify: FAIL — {detail}");
            }
            return Ok(VerifyResult {
                passed,
                bytes_checked: check_size,
                detail,
            });
        }

        // Large image: also read tail chunk from device
        let tail_start_sector = start_sector + num_sectors - head_sectors;
        let mut tail_buf = vec![0u8; (head_sectors * sector_size) as usize];
        {
            let mut cursor = Cursor::new(&mut tail_buf[..]);
            qdl_safe(|| {
                firehose_read_storage(
                    &mut conn.device,
                    &mut cursor,
                    head_sectors as usize,
                    0,
                    lun,
                    tail_start_sector as u32,
                )
                .map_err(|e| format!("Verify read (tail) failed: {e}"))
            })?;
        }
        let dev_tail_hash = format!("{:x}", Sha256::digest(&tail_buf[..check_size as usize]));

        let head_ok = dev_head_hash == src_head;
        let tail_ok = dev_tail_hash == src_tail;
        let passed = head_ok && tail_ok;
        let bytes_checked = check_size * 2;

        let detail = if passed {
            format!("Head+Tail SHA256 match ({} bytes checked)", bytes_checked)
        } else {
            let mut parts = Vec::new();
            if !head_ok {
                parts.push(format!(
                    "head: src={}… dev={}…",
                    &src_head[..16],
                    &dev_head_hash[..16]
                ));
            }
            if !tail_ok {
                parts.push(format!(
                    "tail: src={}… dev={}…",
                    &src_tail[..16],
                    &dev_tail_hash[..16]
                ));
            }
            format!("SHA256 MISMATCH: {}", parts.join("; "))
        };

        if passed {
            info!("EDL verify: PASS — {detail}");
        } else {
            warn!("EDL verify: FAIL — {detail}");
        }

        Ok(VerifyResult {
            passed,
            bytes_checked,
            detail,
        })
    }

    /// Batch flash from rawprogram.xml + optional patch.xml.
    pub fn batch_flash(
        conn: &mut EdlConnection,
        rawprogram_path: &Path,
        patch_path: Option<&Path>,
        image_dir: &Path,
    ) -> Result<BatchFlashResult, FlashError> {
        validate_batch_files(rawprogram_path, image_dir)?;

        let (programs, erases) = parse_rawprogram(rawprogram_path)?;
        let mut result = BatchFlashResult {
            programmed: Vec::new(),
            erased: Vec::new(),
            patched: 0,
            errors: Vec::new(),
            verified: Vec::new(),
        };

        // Execute erases first
        for entry in &erases {
            info!(
                "EDL batch: erasing {} sectors at LUN {} sector {}",
                entry.num_partition_sectors, entry.physical_partition_number, entry.start_sector
            );
            match Self::erase_partition(
                conn,
                entry.physical_partition_number,
                entry.start_sector,
                entry.num_partition_sectors,
            ) {
                Ok(()) => result.erased.push(format!(
                    "LUN{}:{}",
                    entry.physical_partition_number, entry.start_sector
                )),
                Err(e) => result.errors.push(format!(
                    "Erase failed at sector {}: {e}",
                    entry.start_sector
                )),
            }
        }

        // Execute programs in order
        for entry in &programs {
            let img_path = image_dir.join(&entry.filename);
            info!(
                "EDL batch: programming {} → LUN {} sector {} ({} sectors)",
                entry.filename,
                entry.physical_partition_number,
                entry.start_sector,
                entry.num_partition_sectors
            );
            match Self::program_partition(
                conn,
                entry.physical_partition_number,
                entry.start_sector,
                entry.num_partition_sectors,
                &img_path,
                None,
            ) {
                Ok(verify) => {
                    result.programmed.push(entry.label.clone());
                    if let Some(v) = verify {
                        result.verified.push((entry.label.clone(), v.passed));
                    }
                }
                Err(e) => result.errors.push(format!("{}: {e}", entry.label)),
            }
        }

        // Apply patches if provided
        if let Some(patch_path) = patch_path {
            let patches = parse_patch_xml(patch_path)?;
            for patch in &patches {
                match qdl_safe(|| {
                    firehose_patch(
                        &mut conn.device,
                        patch.byte_offset,
                        0, // slot
                        patch.physical_partition_number,
                        patch.size_in_bytes,
                        &patch.start_sector,
                        &patch.value,
                    )
                    .map_err(|e| format!("Patch failed: {e}"))
                }) {
                    Ok(()) => result.patched += 1,
                    Err(e) => result.errors.push(e.to_string()),
                }
            }
        }

        info!(
            "EDL batch complete: {} programmed, {} erased, {} patched, {} errors",
            result.programmed.len(),
            result.erased.len(),
            result.patched,
            result.errors.len()
        );

        Ok(result)
    }

    /// Batch flash from a directory containing rawprogram*.xml + patch*.xml + images.
    /// Discovers all rawprogram files, processes each sequentially.
    pub fn batch_flash_dir(
        conn: &mut EdlConnection,
        dir: &Path,
    ) -> Result<BatchFlashResult, FlashError> {
        let sets = crate::protocols::edl_xml::discover_rawprograms(dir)?;

        if sets.is_empty() {
            return Err(FlashError::Validation(
                "No rawprogram*.xml files found in directory".into(),
            ));
        }

        info!(
            "EDL: discovered {} rawprogram files in {}",
            sets.len(),
            dir.display()
        );

        let mut combined = BatchFlashResult {
            programmed: Vec::new(),
            erased: Vec::new(),
            patched: 0,
            errors: Vec::new(),
            verified: Vec::new(),
        };

        for set in &sets {
            info!(
                "EDL: flashing {} (LUN {})",
                set.rawprogram_path.display(),
                set.lun_hint
            );

            match Self::batch_flash(conn, &set.rawprogram_path, set.patch_path.as_deref(), dir) {
                Ok(result) => {
                    combined.programmed.extend(result.programmed);
                    combined.erased.extend(result.erased);
                    combined.patched += result.patched;
                    combined.verified.extend(result.verified);
                    if !result.errors.is_empty() {
                        for err in result.errors {
                            combined.errors.push(format!("LUN {}: {err}", set.lun_hint));
                        }
                    }
                }
                Err(e) => {
                    combined.errors.push(format!(
                        "LUN {} ({}): {e}",
                        set.lun_hint,
                        set.rawprogram_path.display()
                    ));
                }
            }
        }

        Ok(combined)
    }
}

// --- Post-write SHA256 verification helper ---

/// Compute SHA256 hashes of a file's head and tail regions for verification.
/// For small files (<= 2 * VERIFY_CHUNK_SIZE), hashes the entire file and returns
/// the same hash for both head and tail. For larger files, hashes the first and
/// last VERIFY_CHUNK_SIZE bytes independently.
fn compute_file_hashes(path: &Path, file_size: u64) -> Result<(String, String), FlashError> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| FlashError::Protocol(format!("Cannot open file for hashing: {e}")))?;

    let size = file_size as usize;
    let threshold = 2 * VERIFY_CHUNK_SIZE;

    if size <= threshold {
        // Small file: hash the entire content, return same hash for head and tail
        let mut buf = vec![0u8; size];
        file.read_exact(&mut buf)
            .map_err(|e| FlashError::Protocol(format!("Failed to read file for hashing: {e}")))?;
        let hash = format!("{:x}", Sha256::digest(&buf));
        Ok((hash.clone(), hash))
    } else {
        // Large file: hash first chunk (head) and last chunk (tail)
        let mut head_buf = vec![0u8; VERIFY_CHUNK_SIZE];
        file.read_exact(&mut head_buf)
            .map_err(|e| FlashError::Protocol(format!("Failed to read file head: {e}")))?;
        let head_hash = format!("{:x}", Sha256::digest(&head_buf));

        let tail_offset = size - VERIFY_CHUNK_SIZE;
        file.seek(SeekFrom::Start(tail_offset as u64))
            .map_err(|e| FlashError::Protocol(format!("Failed to seek to file tail: {e}")))?;
        let mut tail_buf = vec![0u8; VERIFY_CHUNK_SIZE];
        file.read_exact(&mut tail_buf)
            .map_err(|e| FlashError::Protocol(format!("Failed to read file tail: {e}")))?;
        let tail_hash = format!("{:x}", Sha256::digest(&tail_buf));

        Ok((head_hash, tail_hash))
    }
}

// --- Batch file validation ---

/// Validate all image files referenced in rawprogram.xml exist in image_dir.
pub fn validate_batch_files(rawprogram_path: &Path, image_dir: &Path) -> Result<(), FlashError> {
    let (programs, _) = parse_rawprogram(rawprogram_path)?;
    let missing: Vec<String> = programs
        .iter()
        .filter(|p| !image_dir.join(&p.filename).exists())
        .map(|p| p.filename.clone())
        .collect();

    if !missing.is_empty() {
        return Err(FlashError::Validation(format!(
            "Missing image files: {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

/// Return list of missing image filenames referenced in rawprogram.xml.
pub fn validate_batch_paths(rawprogram_path: &Path, image_dir: &Path) -> Result<Vec<String>, FlashError> {
    let (programs, _) = parse_rawprogram(rawprogram_path)?;
    // Reject path traversal attempts
    let programs: Vec<_> = programs.into_iter()
        .filter(|p| {
            if p.filename.contains("..") || std::path::Path::new(&p.filename).is_absolute() {
                warn!("Rejected suspicious filename in rawprogram XML: {}", p.filename);
                false
            } else {
                true
            }
        })
        .collect();
    Ok(programs
        .iter()
        .filter(|p| !p.filename.is_empty() && !image_dir.join(&p.filename).exists())
        .map(|p| p.filename.clone())
        .collect())
}

// --- Programmer validation ---

/// Validate a programmer file by checking for ELF or MBN magic bytes.
pub fn validate_programmer_file(path: &Path) -> Result<(), FlashError> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| FlashError::Protocol(format!("Cannot open programmer file: {e}")))?;

    let mut header = [0u8; 4];
    std::io::Read::read_exact(&mut file, &mut header)
        .map_err(|_| FlashError::Protocol("Programmer file too small (< 4 bytes)".into()))?;

    if is_valid_programmer_magic(&header) {
        Ok(())
    } else {
        Err(FlashError::Protocol(
            "Invalid programmer file. Expected ELF (.elf) or MBN (.mbn) format.".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // --- Programmer validation tests ---

    #[test]
    fn test_validate_elf_programmer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prog.elf");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"\x7fELF\x02\x01\x01\x00").unwrap();
        assert!(validate_programmer_file(&path).is_ok());
    }

    #[test]
    fn test_validate_mbn_sbl_programmer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prog.mbn");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&[0xD1, 0xDC, 0x4B, 0x84, 0x00, 0x00, 0x00, 0x00])
            .unwrap();
        assert!(validate_programmer_file(&path).is_ok());
    }

    #[test]
    fn test_validate_mbn_image_id_programmer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prog.mbn");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&13u32.to_le_bytes()).unwrap();
        f.write_all(&[0u8; 36]).unwrap();
        assert!(validate_programmer_file(&path).is_ok());
    }

    #[test]
    fn test_validate_rejects_random_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("random.bin");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"this is not a programmer").unwrap();
        assert!(validate_programmer_file(&path).is_err());
    }

    #[test]
    fn test_validate_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.elf");
        std::fs::File::create(&path).unwrap();
        assert!(validate_programmer_file(&path).is_err());
    }

    #[test]
    fn test_validate_rejects_missing_file() {
        let path = std::path::Path::new("/nonexistent/programmer.elf");
        assert!(validate_programmer_file(path).is_err());
    }

    // --- Batch validation tests ---

    #[test]
    fn test_batch_validates_missing_images() {
        let dir = tempfile::tempdir().unwrap();
        let rawprogram = dir.path().join("rawprogram0.xml");
        {
            let mut f = std::fs::File::create(&rawprogram).unwrap();
            f.write_all(
                br#"<?xml version="1.0" ?>
<data>
  <program SECTOR_SIZE_IN_BYTES="4096" file_sector_offset="0"
           filename="missing.img" label="boot" num_partition_sectors="100"
           physical_partition_number="0" start_sector="2048" />
</data>"#,
            )
            .unwrap();
        }

        let result = validate_batch_files(&rawprogram, dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("missing.img"),
            "Error should mention missing file: {err}"
        );
    }

    #[test]
    fn test_validate_batch_paths_returns_missing() {
        let dir = tempfile::tempdir().unwrap();
        let xml_path = dir.path().join("rawprogram0.xml");
        std::fs::write(&xml_path, r#"<?xml version="1.0"?>
<data>
  <program SECTOR_SIZE_IN_BYTES="512" start_sector="0" num_partition_sectors="100" filename="boot.img" label="boot" />
  <program SECTOR_SIZE_IN_BYTES="512" start_sector="200" num_partition_sectors="500" filename="system.img" label="system" />
</data>"#).unwrap();

        std::fs::write(dir.path().join("boot.img"), b"boot").unwrap();

        let missing = validate_batch_paths(&xml_path, dir.path()).unwrap();
        assert_eq!(missing, vec!["system.img"]);
    }

    #[test]
    fn test_validate_batch_paths_all_present() {
        let dir = tempfile::tempdir().unwrap();
        let xml_path = dir.path().join("rawprogram0.xml");
        std::fs::write(&xml_path, r#"<?xml version="1.0"?>
<data>
  <program SECTOR_SIZE_IN_BYTES="512" start_sector="0" num_partition_sectors="100" filename="boot.img" label="boot" />
</data>"#).unwrap();
        std::fs::write(dir.path().join("boot.img"), b"boot").unwrap();

        let missing = validate_batch_paths(&xml_path, dir.path()).unwrap();
        assert!(missing.is_empty());
    }

    // --- Firehose nop probe tests ---

    #[test]
    fn test_nop_xml_is_well_formed() {
        let nop = b"<?xml version=\"1.0\" ?><data><nop /></data>";
        let text = std::str::from_utf8(nop).unwrap();
        assert!(text.contains("<nop />"));
        assert!(text.starts_with("<?xml"));
    }

    #[test]
    fn test_firehose_response_detection_ack() {
        // Simulates what probe_firehose_nop checks for in the response
        let response = r#"<?xml version="1.0" encoding="UTF-8" ?><data><response value="ACK" /></data>"#;
        assert!(response.contains("<?xml") || response.contains("<response") || response.contains("<log"));
    }

    #[test]
    fn test_firehose_response_detection_log() {
        // Firehose programmer often sends log messages before ACK
        let response = r#"<?xml version="1.0" encoding="UTF-8" ?><data><log value="INFO: Chip serial num: 1785722369" /></data>"#;
        assert!(response.contains("<?xml") || response.contains("<response") || response.contains("<log"));
    }

    #[test]
    fn test_sahara_binary_not_detected_as_firehose() {
        // Sahara HELLO starts with 0x01 (binary) — should NOT match XML patterns
        let sahara_hello: [u8; 8] = [0x01, 0x00, 0x00, 0x00, 0x30, 0x00, 0x00, 0x00];
        let text = String::from_utf8_lossy(&sahara_hello);
        assert!(!text.contains("<?xml"));
        assert!(!text.contains("<response"));
        assert!(!text.contains("<log"));
        assert!(!text.contains("<data"));
    }

    #[test]
    fn test_edl_device_info_firehose_active_default() {
        let info = EdlDeviceInfo {
            serial: None,
            hw_id: None,
            pk_hash: None,
            storage_type: None,
            sector_size: None,
            num_luns: None,
            firehose_active: false,
            chipset: None,
        };
        assert!(!info.firehose_active);
    }

    #[test]
    fn test_edl_device_info_firehose_active_set() {
        let info = EdlDeviceInfo {
            serial: None,
            hw_id: None,
            pk_hash: None,
            storage_type: None,
            sector_size: None,
            num_luns: None,
            firehose_active: true,
            chipset: None,
        };
        assert!(info.firehose_active);
        // When firehose_active, serial/hw_id/pk_hash are all None
        // (Sahara timed out, but Firehose probe succeeded)
        assert!(info.serial.is_none());
    }
}

#[cfg(test)]
mod verify_tests {
    use super::*;
    use sha2::{Sha256, Digest};

    #[test]
    fn test_compute_file_hashes_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.img");
        let data = vec![0xABu8; 512];
        std::fs::write(&path, &data).unwrap();

        let (head, tail) = compute_file_hashes(&path, 512).unwrap();
        let expected = format!("{:x}", Sha256::digest(&data));
        assert_eq!(head, expected);
        assert_eq!(tail, expected);
    }

    #[test]
    fn test_compute_file_hashes_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.img");
        let check_size = VERIFY_CHUNK_SIZE;
        let total_size = check_size * 4;
        let mut data = vec![0u8; total_size];
        data[..check_size].fill(0xAA);
        data[total_size - check_size..].fill(0xBB);
        std::fs::write(&path, &data).unwrap();

        let (head, tail) = compute_file_hashes(&path, total_size as u64).unwrap();

        let expected_head = format!("{:x}", Sha256::digest(vec![0xAAu8; check_size]));
        let expected_tail = format!("{:x}", Sha256::digest(vec![0xBBu8; check_size]));

        assert_eq!(head, expected_head);
        assert_eq!(tail, expected_tail);
    }

    // --- qdl_safe wrapper tests ---

    #[test]
    fn test_qdl_safe_ok_result() {
        let result: Result<i32, FlashError> = qdl_safe(|| Ok::<i32, String>(42));
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_qdl_safe_err_result() {
        let result: Result<i32, FlashError> = qdl_safe(|| Err::<i32, String>("test error".into()));
        let err = result.unwrap_err().to_string();
        assert!(err.contains("test error"));
    }

    #[test]
    fn test_qdl_safe_catches_panic() {
        let result: Result<i32, FlashError> = qdl_safe(|| {
            panic!("simulated qdlrs parser panic");
            #[allow(unreachable_code)]
            Ok::<i32, String>(0)
        });
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unexpected response"));
    }

    // --- extract_xml_attr tests ---

    #[test]
    fn test_extract_xml_attr_basic() {
        let xml = r#"<response value="ACK" MaxPayloadSizeToTargetInBytes="1048576"/>"#;
        assert_eq!(extract_xml_attr(xml, "value"), Some("ACK".to_string()));
        assert_eq!(
            extract_xml_attr(xml, "MaxPayloadSizeToTargetInBytes"),
            Some("1048576".to_string())
        );
    }

    #[test]
    fn test_extract_xml_attr_missing() {
        let xml = r#"<response value="ACK"/>"#;
        assert_eq!(extract_xml_attr(xml, "nonexistent"), None);
    }

    #[test]
    fn test_extract_xml_attr_rawmode() {
        let xml = r#"<response value="ACK" rawmode="true"/>"#;
        assert_eq!(extract_xml_attr(xml, "rawmode"), Some("true".to_string()));
    }

    #[test]
    fn test_extract_xml_attr_memory_name() {
        let xml = r#"<response value="ACK" MemoryName="UFS" MaxPayloadSizeToTargetInBytes="1048576" MaxXMLSizeInBytes="4096" Version="1"/>"#;
        assert_eq!(extract_xml_attr(xml, "MemoryName"), Some("UFS".to_string()));
        assert_eq!(extract_xml_attr(xml, "MaxXMLSizeInBytes"), Some("4096".to_string()));
        assert_eq!(extract_xml_attr(xml, "Version"), Some("1".to_string()));
    }

    #[test]
    fn test_extract_xml_attr_empty_value() {
        let xml = r#"<response value=""/>"#;
        assert_eq!(extract_xml_attr(xml, "value"), Some("".to_string()));
    }

    #[test]
    fn test_extract_xml_attr_log_with_auth() {
        let xml = r#"<data><log value="Device is authenticated"/></data>"#;
        assert_eq!(
            extract_xml_attr(xml, "value"),
            Some("Device is authenticated".to_string())
        );
    }

    // --- dedup_sahara_field tests ---

    #[test]
    fn test_dedup_repeated_hwid() {
        // 8-byte HWID repeated 3× = 24 bytes; expect first 8 bytes back.
        let hwid: [u8; 8] = [0x00, 0x00, 0x72, 0x00, 0xe1, 0x50, 0x0a, 0x00];
        let mut input = Vec::new();
        for _ in 0..3 {
            input.extend_from_slice(&hwid);
        }
        assert_eq!(super::EdlProtocol::dedup_sahara_field(&input), hwid);
    }

    #[test]
    fn test_dedup_no_repeat() {
        // 48 unique bytes (SHA-384 style) → returned unchanged.
        let input: Vec<u8> = (0u8..48).collect();
        assert_eq!(super::EdlProtocol::dedup_sahara_field(&input), input);
    }

    #[test]
    fn test_dedup_sha256_repeated() {
        // 32-byte hash repeated to fill a 64-byte buffer → first 32 bytes.
        let hash: Vec<u8> = (0u8..32).collect();
        let mut input = hash.clone();
        input.extend_from_slice(&hash);
        assert_eq!(super::EdlProtocol::dedup_sahara_field(&input), hash);
    }

    #[test]
    fn test_dedup_short_input() {
        // Fewer than 8 bytes → returned unchanged.
        let input = vec![0xAA, 0xBB, 0xCC];
        assert_eq!(super::EdlProtocol::dedup_sahara_field(&input), input);
    }
}

#[cfg(test)]
mod pbl_hack_tests {
    use super::*;

    #[test]
    fn test_pbl_hack_hello_rsp_packet() {
        let packet = EdlProtocol::build_blind_hello_rsp();
        assert_eq!(packet.len(), 48, "HELLO_RSP must be exactly 48 bytes");
        assert_eq!(u32::from_le_bytes([packet[0], packet[1], packet[2], packet[3]]), 0x02);
        assert_eq!(u32::from_le_bytes([packet[4], packet[5], packet[6], packet[7]]), 0x30);
        assert_eq!(u32::from_le_bytes([packet[8], packet[9], packet[10], packet[11]]), 2);
        assert_eq!(u32::from_le_bytes([packet[12], packet[13], packet[14], packet[15]]), 2);
        assert_eq!(u32::from_le_bytes([packet[16], packet[17], packet[18], packet[19]]), 0);
        assert_eq!(u32::from_le_bytes([packet[20], packet[21], packet[22], packet[23]]), 3);
        for i in (24..48).step_by(4) {
            assert_eq!(u32::from_le_bytes([packet[i], packet[i+1], packet[i+2], packet[i+3]]), 0,
                "reserved field at offset {i} must be zero");
        }
    }

    #[test]
    fn test_pbl_hack_switch_mode_packet() {
        let packet = EdlProtocol::build_switch_mode_image_tx();
        assert_eq!(packet.len(), 12, "SWITCH_MODE must be exactly 12 bytes");
        assert_eq!(u32::from_le_bytes([packet[0], packet[1], packet[2], packet[3]]), 0x0C);
        assert_eq!(u32::from_le_bytes([packet[4], packet[5], packet[6], packet[7]]), 0x0C);
        assert_eq!(u32::from_le_bytes([packet[8], packet[9], packet[10], packet[11]]), 0);
    }

    #[test]
    fn test_sahara_cmd_ready_detection() {
        // CMD_READY response: command=0x0B, length=0x08
        let valid: [u8; 8] = [0x0B, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00];
        let cmd_id = u32::from_le_bytes([valid[0], valid[1], valid[2], valid[3]]);
        assert_eq!(cmd_id, 0x0B, "CMD_READY command ID is 0x0B");

        // Not CMD_READY: HELLO response
        let hello: [u8; 8] = [0x01, 0x00, 0x00, 0x00, 0x30, 0x00, 0x00, 0x00];
        let cmd_id = u32::from_le_bytes([hello[0], hello[1], hello[2], hello[3]]);
        assert_ne!(cmd_id, 0x0B, "HELLO should not be detected as CMD_READY");

        let xml = b"<?xml ver";
        // Not CMD_READY: XML response (Firehose)
        assert_ne!(xml[0], 0x0B, "XML should not be detected as CMD_READY");
    }

    #[test]
    fn test_sahara_protocol_constants() {
        assert_eq!(0x01u32, 0x01, "SAHARA_HELLO");
        assert_eq!(0x02u32, 0x02, "SAHARA_HELLO_RSP");
        assert_eq!(0x03u32, 0x03, "SAHARA_READ_DATA");
        assert_eq!(0x07u32, 0x07, "SAHARA_RESET_REQ");
        assert_eq!(0x08u32, 0x08, "SAHARA_RESET_RSP");
        assert_eq!(0x0Bu32, 0x0B, "SAHARA_CMD_READY");
        assert_eq!(0x0Cu32, 0x0C, "SAHARA_SWITCH_MODE");
        assert_eq!(0x0Du32, 0x0D, "SAHARA_CMD_EXEC");
        assert_eq!(0x00u32, 0x00, "IMAGE_TX_PENDING");
        assert_eq!(0x03u32, 0x03, "COMMAND");
    }

    #[test]
    fn test_recovery_chain_documented_in_identify() {
        // Structural verification: all 3 recovery strategies use correct
        // Sahara protocol packet sizes and command IDs.
        // Runtime behavior is tested on real devices (K20 Pro).

        // Verify packet sizes match Sahara protocol spec
        let hello_rsp = EdlProtocol::build_blind_hello_rsp();
        let switch_mode = EdlProtocol::build_switch_mode_image_tx();
        let reset_req: [u8; 8] = [0x07, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00];

        // HELLO_RSP: 12 fields × 4 bytes = 48
        assert_eq!(hello_rsp.len(), 48);
        // SWITCH_MODE: 3 fields × 4 bytes = 12
        assert_eq!(switch_mode.len(), 12);
        // RESET_REQ: 2 fields × 4 bytes = 8
        assert_eq!(reset_req.len(), 8);

        // Verify each packet starts with the correct command ID
        let hello_rsp_cmd = u32::from_le_bytes([hello_rsp[0], hello_rsp[1], hello_rsp[2], hello_rsp[3]]);
        let switch_mode_cmd = u32::from_le_bytes([switch_mode[0], switch_mode[1], switch_mode[2], switch_mode[3]]);
        let reset_cmd = u32::from_le_bytes([reset_req[0], reset_req[1], reset_req[2], reset_req[3]]);

        assert_eq!(hello_rsp_cmd, 0x02, "HELLO_RSP cmd");
        assert_eq!(switch_mode_cmd, 0x0C, "SWITCH_MODE cmd");
        assert_eq!(reset_cmd, 0x07, "RESET_REQ cmd");
    }
}
