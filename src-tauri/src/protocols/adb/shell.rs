//! ADB Shell V1/V2 protocol, feature detection, interactive shell.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tracing::{error, info};

use crate::error::FlashError;
use crate::types::FlashStage;
use super::{AdbStream, ADB_SERVER_ADDR, adb_connect, emit_progress};
use super::super::adb_usb;

// ─── Shell V2 protocol ─────────────────────────────────────────────────────
// ADB Shell V2: length-prefixed packets with type byte. Binary-safe.
// Requires feature negotiation: device must advertise "shell_v2".
// Spec: docs/superpowers/specs/2026-03-30-shell-v2-design.md

/// Shell V2 header size: 1 byte ID + 4 byte LE length.
const SHELL_V2_HEADER_SIZE: usize = 5;

/// Maximum Shell V2 payload (1 MB, per AOSP MAX_PAYLOAD default).
const SHELL_V2_MAX_PAYLOAD: u32 = 1024 * 1024;

/// Shell V2 packet type IDs (from AOSP shell_protocol.h).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellV2Id {
    Stdin = 0,
    Stdout = 1,
    Stderr = 2,
    Exit = 3,
    CloseStdin = 4,
    WindowSizeChange = 5,
}

impl TryFrom<u8> for ShellV2Id {
    type Error = FlashError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Stdin),
            1 => Ok(Self::Stdout),
            2 => Ok(Self::Stderr),
            3 => Ok(Self::Exit),
            4 => Ok(Self::CloseStdin),
            5 => Ok(Self::WindowSizeChange),
            _ => Err(FlashError::Protocol(format!("Unknown Shell V2 packet ID: {value}"))),
        }
    }
}

/// Result from a shell command. V2 provides exit code and separate stderr; V1 does not.
pub struct ShellResult {
    pub stdout: String,
    #[allow(dead_code)] // Populated by V2; read in tests, callers can opt in
    pub stderr: String,
    #[allow(dead_code)] // Populated by V2; read in tests, callers can opt in
    pub exit_code: Option<u8>,
}

/// Build a Shell V2 packet: [id: u8][length: u32 LE][data].
pub fn shell_v2_build_packet(id: ShellV2Id, data: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(SHELL_V2_HEADER_SIZE + data.len());
    packet.push(id as u8);
    packet.extend_from_slice(&(data.len() as u32).to_le_bytes());
    packet.extend_from_slice(data);
    packet
}

/// Parse a 5-byte Shell V2 header into (id, payload_length).
fn shell_v2_parse_header(header: &[u8; SHELL_V2_HEADER_SIZE]) -> (Result<ShellV2Id, FlashError>, u32) {
    let id = ShellV2Id::try_from(header[0]);
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    (id, len)
}

/// Read one Shell V2 packet from a stream. BLOCKING.
/// Returns (packet_id, payload_data).
pub fn shell_v2_read_packet(stream: &mut AdbStream) -> Result<(ShellV2Id, Vec<u8>), FlashError> {
    let mut header = [0u8; SHELL_V2_HEADER_SIZE];
    stream.read_exact(&mut header).map_err(|e| classify_stream_error(e, "header"))?;

    let (id_result, len) = shell_v2_parse_header(&header);
    let id = id_result?;

    if len > SHELL_V2_MAX_PAYLOAD {
        return Err(FlashError::Protocol(format!(
            "Shell V2 payload too large: {} bytes (max {})", len, SHELL_V2_MAX_PAYLOAD
        )));
    }

    let mut data = vec![0u8; len as usize];
    if len > 0 {
        stream.read_exact(&mut data).map_err(|e| classify_stream_error(e, "data"))?;
    }

    Ok((id, data))
}

/// Classify a stream read error into the appropriate FlashError variant.
/// TimedOut maps to DeviceDisconnected intentionally: our timeouts are generous
/// (30s for commands, 10s between dd chunks). If these expire, the device is
/// genuinely unresponsive — not merely slow.
fn classify_stream_error(e: std::io::Error, phase: &str) -> FlashError {
    match e.kind() {
        std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::UnexpectedEof
        | std::io::ErrorKind::TimedOut
        | std::io::ErrorKind::WouldBlock => FlashError::DeviceDisconnected,
        _ => FlashError::Protocol(format!("Shell V2 {phase} read failed: {e}")),
    }
}

// ─── Shell V2 feature detection ────────────────────────────────────────────

/// Per-device Shell V2 support cache.
/// Populated from CNXN banner (USB) or host-serial features query (TCP).
static SHELL_V2_CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

fn shell_v2_cache() -> &'static Mutex<HashMap<String, bool>> {
    SHELL_V2_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn parse_feature_list(features: &str) -> Vec<&str> {
    features.split(',').map(|f| f.trim()).collect()
}

/// Insert a Shell V2 support entry into the cache.
/// Called by adb_connect() when a USB connection provides banner info.
pub(super) fn shell_v2_cache_insert(serial: &str, has_v2: bool) {
    if let Ok(mut cache) = shell_v2_cache().lock() {
        cache.insert(serial.to_string(), has_v2);
    }
}

/// Check if a device supports Shell V2 (cached). BLOCKING on first check per serial.
pub fn shell_v2_supported(serial: &str, force_usb: bool) -> bool {
    // Check cache first
    if let Ok(cache) = shell_v2_cache().lock() {
        if let Some(&v2) = cache.get(serial) {
            return v2;
        }
    }

    // USB mode: can't query server. Return false (will be set on next CNXN).
    if force_usb {
        return false;
    }

    // Query ADB server for features
    let has_v2 = shell_v2_check_device(serial);

    if let Ok(mut cache) = shell_v2_cache().lock() {
        cache.insert(serial.to_string(), has_v2);
    }

    has_v2
}

/// Query ADB server for device features. Returns the raw feature string. BLOCKING.
fn shell_v2_query_features(query: &str) -> Result<String, FlashError> {
    let mut tcp = TcpStream::connect(ADB_SERVER_ADDR)
        .map_err(|e| FlashError::Protocol(format!("ADB server connect failed: {e}")))?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| FlashError::Protocol(format!("Failed to set timeout: {e}")))?;

    let payload = format!("{:04x}{}", query.len(), query);
    tcp.write_all(payload.as_bytes())
        .map_err(|e| FlashError::Protocol(format!("Feature query write failed: {e}")))?;

    let mut status = [0u8; 4];
    tcp.read_exact(&mut status)
        .map_err(|e| FlashError::Protocol(format!("Feature query read failed: {e}")))?;

    if &status != b"OKAY" {
        return Err(FlashError::Protocol("ADB server returned FAIL for feature query".into()));
    }

    let mut len_buf = [0u8; 4];
    tcp.read_exact(&mut len_buf)
        .map_err(|e| FlashError::Protocol(format!("Feature length read failed: {e}")))?;

    let len = usize::from_str_radix(
        std::str::from_utf8(&len_buf).unwrap_or("0000"), 16
    ).unwrap_or(0);

    let mut features = vec![0u8; len];
    tcp.read_exact(&mut features)
        .map_err(|e| FlashError::Protocol(format!("Feature data read failed: {e}")))?;

    Ok(String::from_utf8_lossy(&features).to_string())
}

/// Check if a device supports Shell V2 by querying ADB server features. BLOCKING.
fn shell_v2_check_device(serial: &str) -> bool {
    // Try device-specific query first
    let query = format!("host-serial:{serial}:features");
    let features = match shell_v2_query_features(&query) {
        Ok(f) => f,
        Err(_) => {
            // Fall back to generic host:features
            match shell_v2_query_features("host:features") {
                Ok(f) => f,
                Err(_) => return false,
            }
        }
    };

    let has_v2 = parse_feature_list(&features).contains(&"shell_v2");
    info!("Device {serial} features: {features} (shell_v2={has_v2})");
    has_v2
}

/// Clear the Shell V2 feature cache (call on device disconnect/reconnect).
pub(super) fn shell_v2_clear_cache() {
    if let Ok(mut cache) = shell_v2_cache().lock() {
        cache.clear();
    }
}

// ─── Shell commands ────────────────────────────────────────────────────────

/// Maximum cumulative output from a shell command (100 MB).
/// Prevents OOM if a device sends unlimited data without an Exit packet.
const SHELL_V2_MAX_OUTPUT: usize = 100 * 1024 * 1024;

/// Run a shell command via Shell V2 protocol. BLOCKING.
/// Returns ShellResult with separate stdout and exit code.
fn adb_shell_v2(serial: &str, command: &str, read_timeout: Option<Duration>, force_usb: bool) -> Result<ShellResult, FlashError> {
    let service = format!("shell,v2,raw:{command}");
    let mut stream = adb_connect(serial, &service, read_timeout, force_usb)?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut total_bytes: usize = 0;

    let exit_code = loop {
        let (id, data) = shell_v2_read_packet(&mut stream)?;
        match id {
            ShellV2Id::Stdout => {
                total_bytes += data.len();
                if total_bytes > SHELL_V2_MAX_OUTPUT {
                    return Err(FlashError::Protocol(format!(
                        "Shell output exceeded {} MB limit", SHELL_V2_MAX_OUTPUT / 1024 / 1024
                    )));
                }
                stdout.extend_from_slice(&data);
            }
            ShellV2Id::Stderr => {
                total_bytes += data.len();
                if total_bytes > SHELL_V2_MAX_OUTPUT {
                    return Err(FlashError::Protocol(format!(
                        "Shell output exceeded {} MB limit", SHELL_V2_MAX_OUTPUT / 1024 / 1024
                    )));
                }
                stderr.extend_from_slice(&data);
            }
            ShellV2Id::Exit => {
                break data.first().copied();
            }
            _ => {}
        }
    };

    Ok(ShellResult {
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
        exit_code,
    })
}

/// Stream a Shell V2 command's stdout directly to a writer. BLOCKING.
/// Used for binary-safe `dd` streaming — no temp file, no device storage needed.
/// Returns the process exit code.
pub(super) fn adb_shell_stream<W: Write>(
    serial: &str,
    command: &str,
    writer: &mut W,
    app: &tauri::AppHandle,
    expected_bytes: Option<u64>,
    progress_label: &str,
    force_usb: bool,
) -> Result<u8, FlashError> {
    let service = format!("shell,v2,raw:{command}");
    // 10s timeout between chunks — dd output is continuous, a 10s gap means disconnect
    let mut stream = adb_connect(serial, &service, Some(Duration::from_secs(10)), force_usb)?;

    let mut received: u64 = 0;
    let mut last_progress: u64 = 0;

    loop {
        let (id, data) = match shell_v2_read_packet(&mut stream) {
            Ok(packet) => packet,
            Err(FlashError::DeviceDisconnected) => {
                adb_usb::invalidate_usb_cache(serial);
                let mb = received as f64 / 1_048_576.0;
                error!("Device disconnected during streaming ({:.1} MB received)", mb);
                emit_progress(
                    app,
                    FlashStage::Error,
                    &format!("Device disconnected ({:.1} MB received)", mb),
                    None,
                );
                return Err(FlashError::DeviceDisconnected);
            }
            Err(e) => return Err(e),
        };
        match id {
            ShellV2Id::Stdout => {
                writer.write_all(&data).map_err(FlashError::Io)?;
                received += data.len() as u64;

                // Emit progress every ~256KB
                if received - last_progress >= 262144 || expected_bytes.is_some_and(|e| received >= e) {
                    last_progress = received;
                    if let Some(expected) = expected_bytes {
                        let percent = (received as f32 / expected as f32 * 100.0).min(100.0);
                        emit_progress(
                            app,
                            FlashStage::Sending,
                            &format!("Reading {}... {:.0}%", progress_label, percent),
                            Some(percent),
                        );
                    } else {
                        let mb = received as f32 / 1_048_576.0;
                        emit_progress(
                            app,
                            FlashStage::Sending,
                            &format!("Reading {}... {:.1} MB", progress_label, mb),
                            None,
                        );
                    }
                }
            }
            ShellV2Id::Stderr => {
                let msg = String::from_utf8_lossy(&data);
                if !msg.trim().is_empty() {
                    info!("dd stderr: {}", msg.trim());
                }
            }
            ShellV2Id::Exit => {
                let code = data.first().copied().unwrap_or(255);
                info!("Shell V2 stream complete: {} bytes, exit code {}", received, code);
                return Ok(code);
            }
            _ => {}
        }
    }
}

/// Stream a local file's contents into a Shell V2 command's stdin. BLOCKING.
/// Used for partition write: `dd of=/dev/block/by-name/{partition}` reads from stdin.
/// Returns the process exit code.
pub(super) fn adb_shell_stream_stdin<R: Read>(
    serial: &str,
    command: &str,
    reader: &mut R,
    total_bytes: u64,
    app: &tauri::AppHandle,
    progress_label: &str,
    force_usb: bool,
) -> Result<u8, FlashError> {
    let service = format!("shell,v2,raw:{command}");
    let mut stream = adb_connect(serial, &service, Some(Duration::from_secs(30)), force_usb)?;

    let mut sent: u64 = 0;
    let mut last_progress: u64 = 0;
    let mut buf = vec![0u8; 65536]; // 64KB chunks — matches dd bs=65536

    loop {
        let n = reader.read(&mut buf).map_err(FlashError::Io)?;
        if n == 0 { break; }

        let packet = shell_v2_build_packet(ShellV2Id::Stdin, &buf[..n]);
        stream.write_all(&packet).map_err(|e| {
            if e.kind() == std::io::ErrorKind::ConnectionReset
                || e.kind() == std::io::ErrorKind::BrokenPipe
            {
                adb_usb::invalidate_usb_cache(serial);
                let mb = sent as f64 / 1_048_576.0;
                error!("Device disconnected during write ({:.1} MB sent)", mb);
                emit_progress(
                    app,
                    FlashStage::Error,
                    &format!("Device disconnected ({:.1} MB sent)", mb),
                    None,
                );
                FlashError::DeviceDisconnected
            } else {
                FlashError::Protocol(format!("Shell V2 stdin write failed: {e}"))
            }
        })?;

        sent += n as u64;

        // Emit progress every ~256KB
        if sent - last_progress >= 262144 || sent >= total_bytes {
            last_progress = sent;
            let percent = (sent as f32 / total_bytes as f32 * 100.0).min(100.0);
            emit_progress(
                app,
                FlashStage::Sending,
                &format!("Writing {}... {:.0}%", progress_label, percent),
                Some(percent),
            );
        }
    }

    // Signal end of stdin
    let close_packet = shell_v2_build_packet(ShellV2Id::CloseStdin, &[]);
    stream.write_all(&close_packet).map_err(|e|
        FlashError::Protocol(format!("Shell V2 CloseStdin failed: {e}"))
    )?;

    // Extend read timeout — dd may take minutes to flush large writes to the block device
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));

    // Read response packets until Exit
    loop {
        let (id, data) = shell_v2_read_packet(&mut stream)?;
        match id {
            ShellV2Id::Stdout | ShellV2Id::Stderr => {
                let msg = String::from_utf8_lossy(&data);
                if !msg.trim().is_empty() {
                    info!("dd output: {}", msg.trim());
                }
            }
            ShellV2Id::Exit => {
                let code = data.first().copied().unwrap_or(255);
                info!("Shell V2 stdin stream complete: {} bytes sent, exit code {}", sent, code);
                return Ok(code);
            }
            _ => {}
        }
    }
}

/// Run a shell command on a device. BLOCKING.
/// Auto-detects Shell V2 support. V2 gives exit codes; V1 is the fallback.
pub(super) fn adb_shell(serial: &str, command: &str, read_timeout: Option<Duration>, force_usb: bool) -> Result<ShellResult, FlashError> {
    if shell_v2_supported(serial, force_usb) {
        return adb_shell_v2(serial, command, read_timeout, force_usb);
    }

    // Shell V1 fallback
    let shell = format!("shell:{command}");
    let mut stream = adb_connect(serial, &shell, read_timeout, force_usb)?;

    // Read with size limit to prevent OOM from malicious/malfunctioning devices.
    // Same 100 MB limit as Shell V2 path (SHELL_V2_MAX_OUTPUT).
    let mut output = Vec::new();
    let mut buf = [0u8; 65536];
    let max_output: usize = 100 * 1024 * 1024;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                output.extend_from_slice(&buf[..n]);
                if output.len() > max_output {
                    return Err(FlashError::Protocol(format!(
                        "Shell V1 output exceeded {} MB limit", max_output / 1024 / 1024
                    )));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(FlashError::Protocol(format!("ADB shell read failed: {e}"))),
        }
    }

    Ok(ShellResult {
        stdout: String::from_utf8_lossy(&output).to_string(),
        stderr: String::new(), // V1 mixes stderr into stdout
        exit_code: None,
    })
}

/// Public wrapper for adb_shell, used by adb_local_command.
pub fn adb_shell_pub(serial: &str, command: &str, read_timeout: Option<Duration>, force_usb: bool) -> Result<ShellResult, FlashError> {
    adb_shell(serial, command, read_timeout, force_usb)
}

// ─── Interactive shell ──────────────────────────────────────────────────────

/// Output message sent from a shell session to the frontend via Channel.
#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind")]
pub enum ShellOutput {
    /// Raw stdout data from the device.
    Data { data: Vec<u8> },
    /// Stderr data (Shell V2 only).
    Stderr { data: Vec<u8> },
    /// Session ended (EOF, error, or closed by user).
    Exit { message: String, code: Option<u8> },
}

/// Open an interactive shell connection to a device. BLOCKING.
/// Returns (reader, writer, is_v2).
/// V2: opens shell,v2,TERM=xterm-256color: for colors and packet framing.
/// V1: opens shell: (raw byte stream, no colors).
pub fn adb_shell_open(serial: &str, force_usb: bool) -> Result<(AdbStream, AdbStream, bool), FlashError> {
    let is_v2 = shell_v2_supported(serial, force_usb);
    let service = if is_v2 { "shell,v2,TERM=xterm-256color:" } else { "shell:" };

    let stream = adb_connect(serial, service, Some(Duration::from_secs(60)), force_usb)?;

    match stream {
        AdbStream::Usb(usb_stream) => {
            // USB: create a lightweight write-only handle from the stream.
            // Reader owns the channel, writer sends WRTE fire-and-forget.
            let writer = usb_stream.create_shell_writer();
            if is_v2 { info!("Shell V2 session opened for {serial} via USB Direct"); }
            else { info!("Shell V1 session opened for {serial} via USB Direct"); }
            Ok((AdbStream::Usb(usb_stream), AdbStream::UsbWriter(writer), is_v2))
        }
        AdbStream::Tcp(tcp) => {
            // TCP: clone for reader/writer split.
            let writer = tcp.try_clone()
                .map_err(|e| FlashError::Protocol(format!("Stream clone failed: {e}")))?;
            if is_v2 { info!("Shell V2 session opened for {serial} via TCP"); }
            else { info!("Shell V1 session opened for {serial} via TCP"); }
            Ok((AdbStream::Tcp(tcp), AdbStream::Tcp(writer), is_v2))
        }
        AdbStream::UsbWriter(_) => {
            Err(FlashError::Protocol("Internal error: UsbWriter cannot be used as shell source".into()))
        }
    }
}

/// Open a Shell V2 raw command session (non-interactive).
/// Used for streaming commands like `logcat -B`.
pub fn adb_shell_open_command(serial: &str, command: &str, force_usb: bool) -> Result<(AdbStream, AdbStream, bool), FlashError> {
    if !shell_v2_supported(serial, force_usb) {
        return Err(FlashError::Protocol("Shell V2 required for logcat streaming".into()));
    }
    let service = format!("shell,v2,raw:{command}");
    let stream = adb_connect(serial, &service, None, force_usb)?;

    match stream {
        AdbStream::Usb(usb_stream) => {
            let writer = usb_stream.create_shell_writer();
            info!("Shell V2 command session opened for {serial} via USB: {command}");
            Ok((AdbStream::Usb(usb_stream), AdbStream::UsbWriter(writer), true))
        }
        AdbStream::Tcp(tcp) => {
            let writer = tcp.try_clone()
                .map_err(|e| FlashError::Protocol(format!("Stream clone failed: {e}")))?;
            info!("Shell V2 command session opened for {serial} via TCP: {command}");
            Ok((AdbStream::Tcp(tcp), AdbStream::Tcp(writer), true))
        }
        AdbStream::UsbWriter(_) => {
            Err(FlashError::Protocol("Internal error: UsbWriter cannot be used as shell source".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_v2_id_from_byte() {
        assert_eq!(ShellV2Id::try_from(0u8), Ok(ShellV2Id::Stdin));
        assert_eq!(ShellV2Id::try_from(1u8), Ok(ShellV2Id::Stdout));
        assert_eq!(ShellV2Id::try_from(2u8), Ok(ShellV2Id::Stderr));
        assert_eq!(ShellV2Id::try_from(3u8), Ok(ShellV2Id::Exit));
        assert_eq!(ShellV2Id::try_from(4u8), Ok(ShellV2Id::CloseStdin));
        assert_eq!(ShellV2Id::try_from(5u8), Ok(ShellV2Id::WindowSizeChange));
        assert!(ShellV2Id::try_from(6u8).is_err());
        assert!(ShellV2Id::try_from(255u8).is_err());
    }

    #[test]
    fn test_shell_v2_build_packet() {
        let packet = shell_v2_build_packet(ShellV2Id::Stdin, b"hello");
        assert_eq!(packet.len(), 5 + 5); // 5-byte header + 5-byte payload
        assert_eq!(packet[0], 0); // Stdin ID
        assert_eq!(u32::from_le_bytes([packet[1], packet[2], packet[3], packet[4]]), 5);
        assert_eq!(&packet[5..], b"hello");
    }

    #[test]
    fn test_shell_v2_build_packet_empty() {
        let packet = shell_v2_build_packet(ShellV2Id::CloseStdin, b"");
        assert_eq!(packet.len(), 5);
        assert_eq!(packet[0], 4); // CloseStdin ID
        assert_eq!(u32::from_le_bytes([packet[1], packet[2], packet[3], packet[4]]), 0);
    }

    #[test]
    fn test_shell_v2_parse_header() {
        let header = [1u8, 10, 0, 0, 0]; // Stdout, length=10
        let (id, len) = shell_v2_parse_header(&header);
        assert_eq!(id, Ok(ShellV2Id::Stdout));
        assert_eq!(len, 10);
    }

    #[test]
    fn test_shell_v2_parse_header_large_payload() {
        // 256 KB payload
        let len_bytes = (256u32 * 1024).to_le_bytes();
        let header = [1u8, len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]];
        let (id, len) = shell_v2_parse_header(&header);
        assert_eq!(id, Ok(ShellV2Id::Stdout));
        assert_eq!(len, 256 * 1024);
    }

    #[test]
    fn test_parse_features_has_shell_v2() {
        let features = "shell_v2,cmd,stat_v2,ls_v2,fixed_push_mkdir,apex";
        assert!(parse_feature_list(features).contains(&"shell_v2"));
    }

    #[test]
    fn test_parse_features_no_shell_v2() {
        let features = "cmd,stat_v2,ls_v2";
        assert!(!parse_feature_list(features).contains(&"shell_v2"));
    }

    #[test]
    fn test_parse_features_empty() {
        let features = "";
        assert!(!parse_feature_list(features).contains(&"shell_v2"));
    }

    #[test]
    fn test_shell_v2_exit_packet() {
        let packet = shell_v2_build_packet(ShellV2Id::Exit, &[0]);
        assert_eq!(packet, vec![3, 1, 0, 0, 0, 0]); // id=3, len=1, data=0
    }

    #[test]
    fn test_shell_v2_window_resize() {
        // Binary winsize struct: 4x u16 LE (rows, cols, xpixel, ypixel)
        let rows: u16 = 24;
        let cols: u16 = 80;
        let mut resize_data = Vec::with_capacity(8);
        resize_data.extend_from_slice(&rows.to_le_bytes());
        resize_data.extend_from_slice(&cols.to_le_bytes());
        resize_data.extend_from_slice(&0u16.to_le_bytes());
        resize_data.extend_from_slice(&0u16.to_le_bytes());
        let packet = shell_v2_build_packet(ShellV2Id::WindowSizeChange, &resize_data);
        assert_eq!(packet[0], 5); // WindowSizeChange
        let len = u32::from_le_bytes([packet[1], packet[2], packet[3], packet[4]]);
        assert_eq!(len, 8); // 4 x u16 = 8 bytes
        assert_eq!(packet[5], 24); assert_eq!(packet[6], 0); // rows=24 LE
        assert_eq!(packet[7], 80); assert_eq!(packet[8], 0); // cols=80 LE
    }

    #[test]
    fn test_shell_v2_binary_data() {
        // Verify binary data passes through unchanged (the whole point of V2)
        let binary: Vec<u8> = (0..=255).collect();
        let packet = shell_v2_build_packet(ShellV2Id::Stdout, &binary);
        assert_eq!(packet[0], 1); // Stdout
        let len = u32::from_le_bytes([packet[1], packet[2], packet[3], packet[4]]);
        assert_eq!(len, 256);
        assert_eq!(&packet[5..], &binary[..]);
    }

    #[test]
    fn test_shell_v2_close_stdin() {
        let packet = shell_v2_build_packet(ShellV2Id::CloseStdin, &[]);
        assert_eq!(packet, vec![4, 0, 0, 0, 0]); // id=4, len=0
    }

    #[test]
    fn test_shell_result_v1_no_exit_code() {
        let result = ShellResult { stdout: "output".to_string(), stderr: String::new(), exit_code: None };
        assert!(result.exit_code.is_none());
        assert!(result.stderr.is_empty());
    }

    #[test]
    fn test_shell_result_v2_with_exit_code() {
        let result = ShellResult { stdout: "".to_string(), stderr: "error msg".to_string(), exit_code: Some(127) };
        assert_eq!(result.exit_code, Some(127));
        assert_eq!(result.stderr, "error msg");
    }
}
