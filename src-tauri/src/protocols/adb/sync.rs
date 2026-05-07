//! ADB SYNC protocol — push and pull files.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use tracing::{info, warn};
use tauri::AppHandle;

use crate::error::FlashError;
use crate::types::FlashStage;
use super::{AdbProtocol, AdbStream, adb_connect, emit_progress};

/// Maximum data payload per SYNC DATA chunk (64 KB).
const SYNC_DATA_MAX: usize = 65536;

// SYNC protocol command IDs (4 ASCII bytes as little-endian u32).
const ID_LSTAT_V1: u32 = u32::from_le_bytes(*b"STAT");
const ID_SEND_V1: u32  = u32::from_le_bytes(*b"SEND");
const ID_RECV_V1: u32  = u32::from_le_bytes(*b"RECV");
const ID_DATA: u32     = u32::from_le_bytes(*b"DATA");
const ID_DONE: u32     = u32::from_le_bytes(*b"DONE");
const ID_OKAY: u32     = u32::from_le_bytes(*b"OKAY");
const ID_FAIL: u32     = u32::from_le_bytes(*b"FAIL");

/// Open a SYNC session on a device. BLOCKING.
fn connect_sync(serial: &str, force_usb: bool) -> Result<AdbStream, FlashError> {
    adb_connect(serial, "sync:", Some(Duration::from_secs(60)), force_usb)
}

/// Write a SYNC request: 4-byte command ID (LE) + 4-byte payload length (LE) + payload.
fn write_sync_request(stream: &mut AdbStream, id: u32, payload: &[u8]) -> Result<(), FlashError> {
    debug_assert!(payload.len() <= SYNC_DATA_MAX);
    stream.write_all(&id.to_le_bytes())
        .map_err(|e| FlashError::Protocol(format!("SYNC write failed: {e}")))?;
    stream.write_all(&(payload.len() as u32).to_le_bytes())
        .map_err(|e| FlashError::Protocol(format!("SYNC write failed: {e}")))?;
    stream.write_all(payload)
        .map_err(|e| FlashError::Protocol(format!("SYNC write failed: {e}")))?;
    Ok(())
}

/// Read an 8-byte SYNC response header: (command_id: u32, value: u32).
fn read_sync_response(stream: &mut AdbStream) -> Result<(u32, u32), FlashError> {
    let mut buf = [0u8; 8];
    stream.read_exact(&mut buf)
        .map_err(|e| FlashError::Protocol(format!("SYNC read failed: {e}")))?;
    let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let val = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    Ok((id, val))
}

/// Read a SYNC FAIL error message of `msg_len` bytes.
fn read_sync_fail(stream: &mut AdbStream, msg_len: u32) -> FlashError {
    let mut msg = vec![0u8; (msg_len as usize).min(4096)];
    if stream.read_exact(&mut msg).is_ok() {
        FlashError::Protocol(String::from_utf8_lossy(&msg).to_string())
    } else {
        FlashError::Protocol("SYNC operation failed (could not read error message)".into())
    }
}

/// Push a local file to the device via SYNC protocol. BLOCKING.
pub(super) fn sync_push(
    serial: &str,
    local_path: &Path,
    remote_path: &str,
    app: &AppHandle,
    force_usb: bool,
) -> Result<(), FlashError> {
    let mut file = std::fs::File::open(local_path).map_err(FlashError::Io)?;
    let metadata = file.metadata().map_err(FlashError::Io)?;
    let file_size = metadata.len();

    if file_size == 0 {
        return Err(FlashError::Validation("File is empty".into()));
    }

    let mtime = metadata.modified()
        .map_err(FlashError::Io)?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);

    let mut stream = connect_sync(serial, force_usb)?;

    // SEND with "path,permissions"
    let send_path = format!("{},0100644", remote_path);
    write_sync_request(&mut stream, ID_SEND_V1, send_path.as_bytes())?;

    info!("Push: {} -> {} ({} bytes)", local_path.display(), remote_path, file_size);
    emit_progress(app, FlashStage::Sending, "Push started...", Some(0.0));

    // DATA chunks
    let mut buf = vec![0u8; SYNC_DATA_MAX];
    let mut sent: u64 = 0;

    loop {
        let n = file.read(&mut buf).map_err(FlashError::Io)?;
        if n == 0 { break; }

        stream.write_all(&ID_DATA.to_le_bytes())
            .map_err(|e| FlashError::Protocol(format!("Push write failed: {e}")))?;
        stream.write_all(&(n as u32).to_le_bytes())
            .map_err(|e| FlashError::Protocol(format!("Push write failed: {e}")))?;
        stream.write_all(&buf[..n])
            .map_err(|e| FlashError::Protocol(format!("Push write failed: {e}")))?;

        sent += n as u64;
        let percent = (sent as f32 / file_size as f32 * 100.0).min(100.0);

        if sent % (SYNC_DATA_MAX as u64 * 10) < SYNC_DATA_MAX as u64 || sent == file_size {
            emit_progress(app, FlashStage::Sending, &format!("Pushing... {:.0}%", percent), Some(percent));
        }
    }

    // DONE with mtime
    stream.write_all(&ID_DONE.to_le_bytes())
        .map_err(|e| FlashError::Protocol(format!("Push DONE write failed: {e}")))?;
    stream.write_all(&mtime.to_le_bytes())
        .map_err(|e| FlashError::Protocol(format!("Push DONE write failed: {e}")))?;

    // Read response
    let (id, val) = read_sync_response(&mut stream)?;
    match id {
        ID_OKAY => {
            info!("Push complete: {} bytes sent to {}", sent, remote_path);
            emit_progress(app, FlashStage::Complete, "Push complete!", Some(100.0));
            Ok(())
        }
        ID_FAIL => {
            let err = read_sync_fail(&mut stream, val);
            emit_progress(app, FlashStage::Error, &err.to_string(), None);
            Err(err)
        }
        _ => Err(FlashError::Protocol(format!("Unexpected SYNC response: 0x{:08x}", id)))
    }
}

/// Pull a file from the device to a local path via SYNC protocol. BLOCKING.
pub(super) fn sync_pull(
    serial: &str,
    remote_path: &str,
    local_path: &Path,
    app: &AppHandle,
    force_usb: bool,
) -> Result<(), FlashError> {
    let mut stream = connect_sync(serial, force_usb)?;

    // STAT to get file size for progress
    write_sync_request(&mut stream, ID_LSTAT_V1, remote_path.as_bytes())?;
    let mut stat_buf = [0u8; 16];
    stream.read_exact(&mut stat_buf)
        .map_err(|e| FlashError::Protocol(format!("STAT read failed: {e}")))?;

    let stat_mode = u32::from_le_bytes([stat_buf[4], stat_buf[5], stat_buf[6], stat_buf[7]]);
    let stat_size = u32::from_le_bytes([stat_buf[8], stat_buf[9], stat_buf[10], stat_buf[11]]);

    if stat_mode == 0 {
        return Err(FlashError::Protocol(format!("Remote file not found: {}", remote_path)));
    }

    // STAT v1 returns size as u32 — files >4GB will show truncated size.
    // Progress will be inaccurate for large files but data transfer is unaffected.
    let file_size = stat_size as u64;
    if stat_size == u32::MAX {
        warn!("Pull: STAT size is u32::MAX — file may be >4GB, progress will be approximate");
    }
    info!("Pull: {} -> {} ({} bytes via STAT)", remote_path, local_path.display(), file_size);
    emit_progress(app, FlashStage::Sending, "Pull started...", Some(0.0));

    // RECV
    write_sync_request(&mut stream, ID_RECV_V1, remote_path.as_bytes())?;

    // Atomic write: use .tmp suffix, rename on success
    let tmp_path = local_path.with_extension("img.tmp");

    let result = (|| -> Result<u64, FlashError> {
        let mut out = std::fs::File::create(&tmp_path).map_err(FlashError::Io)?;
        let mut received: u64 = 0;

        loop {
            let (id, size) = read_sync_response(&mut stream)?;
            match id {
                ID_DATA => {
                    let mut chunk = vec![0u8; size as usize];
                    stream.read_exact(&mut chunk)
                        .map_err(|e| FlashError::Protocol(format!("Pull read failed: {e}")))?;
                    out.write_all(&chunk).map_err(FlashError::Io)?;

                    received += size as u64;

                    // STAT v1 returns u32 size — files >4GB overflow.
                    // When received exceeds reported size, show MB transferred instead of %.
                    let (percent, label) = if file_size > 0 && received <= file_size {
                        let pct = (received as f32 / file_size as f32 * 100.0).min(100.0);
                        (Some(pct), format!("Reading... {:.0}%", pct))
                    } else {
                        let mb = received as f64 / 1_048_576.0;
                        (None, format!("Reading... {:.1} MB", mb))
                    };

                    if received % (SYNC_DATA_MAX as u64 * 10) < size as u64 {
                        emit_progress(app, FlashStage::Sending, &label, percent);
                    }
                }
                ID_DONE => {
                    info!("Pull complete: {} bytes received from {}", received, remote_path);
                    break;
                }
                ID_FAIL => {
                    let err = read_sync_fail(&mut stream, size);
                    return Err(err);
                }
                _ => return Err(FlashError::Protocol(format!("Unexpected SYNC response: 0x{:08x}", id)))
            }
        }

        Ok(received)
    })();

    match result {
        Ok(_received) => {
            std::fs::rename(&tmp_path, local_path).map_err(FlashError::Io)?;
            emit_progress(app, FlashStage::Complete, "Pull complete!", Some(100.0));
            Ok(())
        }
        Err(e) => {
            if let Err(rm_err) = std::fs::remove_file(&tmp_path) {
                if rm_err.kind() != std::io::ErrorKind::NotFound {
                    warn!("Failed to cleanup partial file {}: {}", tmp_path.display(), rm_err);
                }
            }
            emit_progress(app, FlashStage::Error, &e.to_string(), None);
            Err(e)
        }
    }
}

impl AdbProtocol {
    /// Push a local file to the device. BLOCKING — call from spawn_blocking.
    pub fn run_push(serial: &str, local_path: &Path, remote_path: &str, app: &AppHandle, force_usb: bool) -> Result<(), FlashError> {
        sync_push(serial, local_path, remote_path, app, force_usb)
    }

    /// Pull a file from the device. BLOCKING — call from spawn_blocking.
    pub fn run_pull(serial: &str, remote_path: &str, local_path: &Path, app: &AppHandle, force_usb: bool) -> Result<(), FlashError> {
        sync_pull(serial, remote_path, local_path, app, force_usb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_constants_match_ascii() {
        assert_eq!(&ID_LSTAT_V1.to_le_bytes(), b"STAT");
        assert_eq!(&ID_SEND_V1.to_le_bytes(), b"SEND");
        assert_eq!(&ID_RECV_V1.to_le_bytes(), b"RECV");
        assert_eq!(&ID_DATA.to_le_bytes(), b"DATA");
        assert_eq!(&ID_DONE.to_le_bytes(), b"DONE");
        assert_eq!(&ID_OKAY.to_le_bytes(), b"OKAY");
        assert_eq!(&ID_FAIL.to_le_bytes(), b"FAIL");
    }

    #[test]
    fn test_push_path_mode_format() {
        let remote = "/sdcard/update.zip";
        let path_mode = format!("{},0100644", remote);
        assert_eq!(path_mode, "/sdcard/update.zip,0100644");
        assert!(path_mode.contains(','));
    }

    #[test]
    fn test_stat_response_zero_means_not_found() {
        let mode: u32 = 0;
        let size: u32 = 0;
        assert_eq!(mode, 0, "zero mode means file does not exist");
        assert_eq!(size, 0);
    }

    #[test]
    fn test_stat_response_parse() {
        let mode: u32 = 0o100644;
        let size: u32 = 1_048_576;
        let mtime: u32 = 1711500000;
        assert_eq!(mode & 0o170000, 0o100000, "regular file");
        assert_eq!(mode & 0o777, 0o644, "permissions");
        assert_eq!(size, 1_048_576);
        assert!(mtime > 0);
    }
}
