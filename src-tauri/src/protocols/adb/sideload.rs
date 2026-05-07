//! ADB sideload-host protocol for recovery ZIP transfer.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use tracing::{info, warn};
use tauri::AppHandle;

use crate::error::FlashError;
use crate::types::FlashStage;
use super::{AdbProtocol, adb_connect, emit_progress};

/// Sideload block size (64 KB) — matches AOSP SIDELOAD_HOST_BLOCK_SIZE.
const SIDELOAD_BLOCK_SIZE: u64 = 65536;

#[derive(Debug, PartialEq)]
enum SideloadRequest {
    Block(u64),
    Done,
    Fail,
}

fn parse_block_request(buf: &[u8; 8]) -> Result<SideloadRequest, FlashError> {
    if buf == b"DONEDONE" {
        return Ok(SideloadRequest::Done);
    }
    if buf == b"FAILFAIL" {
        return Ok(SideloadRequest::Fail);
    }
    let s = std::str::from_utf8(buf)
        .map_err(|_| FlashError::Protocol("Invalid block request: not UTF-8".into()))?;
    let trimmed = s.trim_start_matches('0');
    let block: u64 = if trimmed.is_empty() {
        0 // "00000000" -> block 0
    } else {
        trimmed.parse().map_err(|_| FlashError::Protocol(
            format!("Invalid block number: {:?}", s)
        ))?
    };
    Ok(SideloadRequest::Block(block))
}

fn calculate_block_read(block: u64, file_size: u64, block_size: u64) -> (u64, usize) {
    let offset = block * block_size;
    let remaining = file_size.saturating_sub(offset);
    let read_len = std::cmp::min(block_size, remaining) as usize;
    (offset, read_len)
}

/// Transfer a ZIP file to a device in recovery sideload mode via ADB sideload-host protocol.
/// BLOCKING — must be called inside spawn_blocking.
fn adb_sideload(serial: &str, file_path: &Path, app: &AppHandle, force_usb: bool) -> Result<(), FlashError> {
    let mut file = std::fs::File::open(file_path)
        .map_err(FlashError::Io)?;
    let file_size = file.metadata()
        .map_err(FlashError::Io)?.len();

    if file_size == 0 {
        return Err(FlashError::Validation("Sideload file is empty".into()));
    }

    let total_blocks = file_size.div_ceil(SIDELOAD_BLOCK_SIZE);

    info!("Sideload: {} bytes, {} blocks of {} bytes",
          file_size, total_blocks, SIDELOAD_BLOCK_SIZE);

    let service = format!("sideload-host:{}:{}", file_size, SIDELOAD_BLOCK_SIZE);
    let mut stream = adb_connect(serial, &service, Some(Duration::from_secs(60)), force_usb)?;

    info!("Sideload service opened, serving blocks...");
    emit_progress(app, FlashStage::Sending, "Sideload started...", Some(0.0));

    let mut blocks_served: u64 = 0;
    let mut buf = vec![0u8; SIDELOAD_BLOCK_SIZE as usize];

    loop {
        let mut req = [0u8; 8];
        stream.read_exact(&mut req).map_err(|e| {
            if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock {
                FlashError::Protocol("Sideload timed out waiting for device".into())
            } else if e.kind() == std::io::ErrorKind::UnexpectedEof {
                if blocks_served == 0 {
                    FlashError::Protocol(
                        "Device is not ready for sideload. Enter Recovery and select 'Apply update from ADB'".into()
                    )
                } else {
                    FlashError::Protocol(format!("Device disconnected after {} blocks: {e}", blocks_served))
                }
            } else {
                FlashError::Protocol(format!("Device disconnected during sideload: {e}"))
            }
        })?;

        match parse_block_request(&req)? {
            SideloadRequest::Done => {
                info!("Sideload complete — device sent DONEDONE");
                emit_progress(app, FlashStage::Complete, "Sideload complete!", Some(100.0));
                return Ok(());
            }
            SideloadRequest::Fail => {
                warn!("Sideload failed — device sent FAILFAIL");
                emit_progress(app, FlashStage::Error, "Device rejected the update package", None);
                return Err(FlashError::Protocol("Device rejected the update package".into()));
            }
            SideloadRequest::Block(block) => {
                let (offset, read_len) = calculate_block_read(block, file_size, SIDELOAD_BLOCK_SIZE);

                if read_len == 0 {
                    return Err(FlashError::Protocol(format!(
                        "Device requested block {} beyond file end (file is {} bytes)", block, file_size
                    )));
                }

                file.seek(SeekFrom::Start(offset))
                    .map_err(FlashError::Io)?;
                file.read_exact(&mut buf[..read_len])
                    .map_err(FlashError::Io)?;
                stream.write_all(&buf[..read_len])
                    .map_err(|e| FlashError::Protocol(format!("Failed to send block {block}: {e}")))?;

                blocks_served += 1;
                let percent = (blocks_served as f32 / total_blocks as f32 * 100.0).min(100.0);

                if blocks_served.is_multiple_of(10) || blocks_served == total_blocks {
                    emit_progress(
                        app,
                        FlashStage::Sending,
                        &format!("Sideloading... {:.0}%", percent),
                        Some(percent),
                    );
                }
            }
        }
    }
}

impl AdbProtocol {
    /// Run ADB sideload transfer. BLOCKING — call from spawn_blocking.
    pub fn run_sideload(serial: &str, file_path: &Path, app: &AppHandle, force_usb: bool) -> Result<(), FlashError> {
        adb_sideload(serial, file_path, app, force_usb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_block_request_block_zero() {
        let buf = *b"00000000";
        assert_eq!(parse_block_request(&buf).unwrap(), SideloadRequest::Block(0));
    }

    #[test]
    fn test_parse_block_request_block_number() {
        let buf = *b"00000023";
        assert_eq!(parse_block_request(&buf).unwrap(), SideloadRequest::Block(23));
    }

    #[test]
    fn test_parse_block_request_done() {
        let buf = *b"DONEDONE";
        assert_eq!(parse_block_request(&buf).unwrap(), SideloadRequest::Done);
    }

    #[test]
    fn test_parse_block_request_fail() {
        let buf = *b"FAILFAIL";
        assert_eq!(parse_block_request(&buf).unwrap(), SideloadRequest::Fail);
    }

    #[test]
    fn test_parse_block_request_invalid_returns_error() {
        let buf = *b"ABCD1234";
        assert!(parse_block_request(&buf).is_err());
    }

    #[test]
    fn test_calculate_block_read_full_block() {
        let (offset, len) = calculate_block_read(0, 131072, 65536);
        assert_eq!(offset, 0);
        assert_eq!(len, 65536);
    }

    #[test]
    fn test_calculate_block_read_partial_last_block() {
        let (offset, len) = calculate_block_read(1, 100000, 65536);
        assert_eq!(offset, 65536);
        assert_eq!(len, 34464);
    }

    #[test]
    fn test_calculate_block_read_beyond_eof() {
        let (offset, len) = calculate_block_read(5, 65536, 65536);
        assert_eq!(offset, 327680);
        assert_eq!(len, 0);
    }
}
