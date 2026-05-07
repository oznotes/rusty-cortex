//! ADB partition dump and raw block device dump.

use std::path::Path;
use std::time::Duration;

use tracing::{info, warn};
use tauri::AppHandle;

use crate::error::FlashError;
use crate::types::{FlashStage, PartitionInfo, RootType};
use super::{AdbProtocol, ADB_SHELL_TIMEOUT, emit_progress};
use super::shell::{adb_shell, adb_shell_stream, adb_shell_stream_stdin, shell_v2_supported};
use super::sync::{sync_pull, sync_push};

// --- DeviceTempGuard ---

/// RAII guard that removes a temp file from the device when dropped.
/// Call `.disarm()` after successful cleanup to prevent double-removal.
pub(super) struct DeviceTempGuard {
    serial: String,
    temp_path: String,
    root_type: RootType,
    force_usb: bool,
    armed: bool,
}

impl DeviceTempGuard {
    pub(super) fn new(serial: &str, temp_path: String, root_type: &RootType, force_usb: bool) -> Self {
        Self {
            serial: serial.to_string(),
            temp_path,
            root_type: root_type.clone(),
            force_usb,
            armed: true,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DeviceTempGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let rm_cmd = AdbProtocol::wrap_for_root(
            &format!("rm {}", self.temp_path),
            &self.root_type,
        );
        match adb_shell(&self.serial, &rm_cmd, ADB_SHELL_TIMEOUT, self.force_usb) {
            Ok(_) => info!("Guard cleaned up device temp: {}", self.temp_path),
            Err(e) => warn!("Guard cleanup failed (device may be disconnected): {}", e),
        }
    }
}

// ─── Partition/dump utilities ──────────────────────────────────────────────

impl AdbProtocol {
    /// Validate a partition name contains only safe characters.
    fn validate_partition_name(name: &str) -> Result<(), FlashError> {
        if name.is_empty() {
            return Err(FlashError::Validation("Partition name is empty".into()));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
            return Err(FlashError::Validation(format!(
                "Partition name contains unsafe characters: {}", name
            )));
        }
        Ok(())
    }

    /// Candidate writable directories on the device, probed in order.
    const TEMP_CANDIDATES: [&'static str; 3] = [
        "/data/local/tmp",
        "/sdcard",
        "/tmp",
    ];

    /// Probe for the first writable temp directory on the device.
    pub fn find_writable_temp(serial: &str, root_type: &RootType, force_usb: bool) -> String {
        for candidate in Self::TEMP_CANDIDATES {
            let probe = format!("test -w {candidate} && echo WRITABLE");
            let probe = Self::wrap_for_root(&probe, root_type);
            if let Ok(result) = adb_shell(serial, &probe, ADB_SHELL_TIMEOUT, force_usb) {
                if result.stdout.trim().contains("WRITABLE") {
                    info!("Writable temp dir: {}", candidate);
                    return candidate.to_string();
                }
            }
        }
        warn!("No writable temp dir detected, falling back to /data/local/tmp");
        "/data/local/tmp".to_string()
    }

    /// Check available space on a device path in bytes. BLOCKING.
    pub fn check_device_space(serial: &str, path: &str, force_usb: bool) -> Option<u64> {
        if let Err(e) = Self::validate_shell_path(path) {
            warn!("check_device_space: {e}");
            return None;
        }
        let cmd = format!("df {path} 2>/dev/null | tail -1");
        let output = adb_shell(serial, &cmd, ADB_SHELL_TIMEOUT, force_usb).ok()?.stdout;
        let fields: Vec<&str> = output.split_whitespace().collect();
        if fields.len() >= 4 {
            fields[3].parse::<u64>().ok().map(|kb| kb * 1024)
        } else {
            None
        }
    }

    /// Generate temp file path using a specific directory.
    pub fn temp_path_in(temp_dir: &str, name: &str) -> String {
        format!("{temp_dir}/.rusty_dump_{name}.img")
    }

    /// Generate temp file path on device for a partition dump.
    #[allow(dead_code)] // Used in tests; kept as public API
    pub fn temp_path(name: &str) -> String {
        Self::temp_path_in("/sdcard", name)
    }

    /// Generate temp file path on device for a partition write (V1 fallback).
    fn write_temp_path_in(temp_dir: &str, name: &str) -> String {
        format!("{temp_dir}/.rusty_write_{name}.img")
    }

    /// Parse batch partition listing output. Each line: "name size_bytes".
    pub fn parse_batch_partition_output(output: &str) -> Vec<PartitionInfo> {
        output
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                let mut parts = line.splitn(2, ' ');
                let name = parts.next()?.trim().to_string();
                let size_str = parts.next().unwrap_or("").trim();

                if name.is_empty() {
                    return None;
                }

                if Self::validate_partition_name(&name).is_err() {
                    warn!("Skipping partition with unsafe name: {:?}", name);
                    return None;
                }

                let size_bytes = size_str.parse::<u64>().ok().filter(|&s| s > 0);
                let size_display = match size_bytes {
                    Some(b) => Self::format_size(b),
                    None => "?".into(),
                };

                Some(PartitionInfo {
                    name,
                    size_bytes,
                    size_display,
                })
            })
            .collect()
    }

    /// List partitions from device. BLOCKING.
    pub fn list_partitions_sync(serial: &str, root_type: &RootType, force_usb: bool) -> Result<Vec<PartitionInfo>, FlashError> {
        let batch_cmd = concat!(
            "for p in $(ls /dev/block/by-name/); do ",
            "s=$(blockdev --getsize64 /dev/block/by-name/$p 2>/dev/null || echo 0); ",
            "echo \"$p $s\"; ",
            "done"
        );
        let batch_cmd = Self::wrap_for_root(batch_cmd, root_type);
        let output = adb_shell(serial, &batch_cmd, ADB_SHELL_TIMEOUT, force_usb)?.stdout;

        let mut partitions = Self::parse_batch_partition_output(&output);
        partitions.sort_by(|a, b| a.name.cmp(&b.name));

        info!("Found {} partitions on device (batch)", partitions.len());
        Ok(partitions)
    }

    /// Build a dd command string with optional offset and size.
    /// Offset rounds DOWN to 4KB block boundary (dd `skip` works in blocks).
    /// Size rounds UP to ensure the requested range is fully covered.
    /// Non-aligned offsets are snapped to the previous block boundary by design.
    pub fn build_dd_command(
        input: &str,
        output: &str,
        offset_bytes: Option<u64>,
        size_bytes: Option<u64>,
    ) -> String {
        let mut cmd = format!("dd if={input} of={output} bs=4096");
        if let Some(offset) = offset_bytes {
            let skip = offset / 4096; // rounds down to block boundary
            if skip > 0 {
                cmd.push_str(&format!(" skip={skip}"));
            }
        }
        if let Some(size) = size_bytes {
            let count = size.div_ceil(4096);
            cmd.push_str(&format!(" count={count}"));
        }
        cmd
    }

    /// Remove any orphaned .rusty_dump_*.img files from a temp dir on the device.
    fn cleanup_device_temps(serial: &str, temp_dir: &str, root_type: &RootType, force_usb: bool) {
        let ls_cmd = format!("ls {temp_dir}/.rusty_dump_*.img 2>/dev/null");
        let ls_cmd = Self::wrap_for_root(&ls_cmd, root_type);

        let files = match adb_shell(serial, &ls_cmd, ADB_SHELL_TIMEOUT, force_usb) {
            Ok(result) => result.stdout
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| l.contains(".rusty_dump_") && l.ends_with(".img"))
                .collect::<Vec<_>>(),
            Err(_) => return,
        };

        if files.is_empty() {
            return;
        }

        info!("Cleaning up {} orphaned temp files on device", files.len());
        for file in &files {
            let rm_cmd = Self::wrap_for_root(&format!("rm {file}"), root_type);
            match adb_shell(serial, &rm_cmd, ADB_SHELL_TIMEOUT, force_usb) {
                Ok(_) => info!("Removed orphaned temp: {}", file),
                Err(e) => warn!("Failed to remove orphaned temp: {} - {}", file, e),
            }
        }
    }

    /// Dump a single partition to a local file. BLOCKING.
    #[allow(clippy::too_many_arguments)]
    pub fn run_dump_partition(
        serial: &str,
        partition: &str,
        local_path: &Path,
        root_type: &RootType,
        temp_dir: &str,
        app: &AppHandle,
        progress_label: &str,
        force_usb: bool,
    ) -> Result<(), FlashError> {
        Self::validate_partition_name(partition)?;
        Self::validate_shell_path(temp_dir)?;

        // Shell V2: stream dd directly to local file
        if shell_v2_supported(serial, force_usb) {
            let dd_cmd = format!("dd if=/dev/block/by-name/{partition} bs=65536");
            let dd_cmd = Self::wrap_for_root(&dd_cmd, root_type);

            emit_progress(app, FlashStage::Sending, &format!("Reading {progress_label}..."), Some(0.0));
            info!("Dump V2 stream: {}", dd_cmd);

            let tmp_path = local_path.with_extension("img.tmp");
            let mut file = std::fs::File::create(&tmp_path).map_err(FlashError::Io)?;

            let exit_code = match adb_shell_stream(serial, &dd_cmd, &mut file, app, None, progress_label, force_usb) {
                Ok(code) => code,
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp_path);
                    if e != FlashError::DeviceDisconnected {
                        emit_progress(app, FlashStage::Error, &format!("Dump failed: {e}"), None);
                    }
                    return Err(e);
                }
            };

            if exit_code != 0 {
                let _ = std::fs::remove_file(&tmp_path);
                emit_progress(app, FlashStage::Error, &format!("dd failed with exit code {exit_code}"), None);
                return Err(FlashError::Protocol(format!("dd failed with exit code {exit_code}")));
            }

            std::fs::rename(&tmp_path, local_path).map_err(FlashError::Io)?;
            emit_progress(app, FlashStage::Complete, &format!("{progress_label} complete!"), Some(100.0));
            return Ok(());
        }

        // Shell V1 fallback: dd to temp file on device, SYNC pull, cleanup
        Self::cleanup_device_temps(serial, temp_dir, root_type, force_usb);

        let temp = Self::temp_path_in(temp_dir, partition);
        let dd_cmd = Self::build_dd_command(
            &format!("/dev/block/by-name/{partition}"),
            &temp,
            None,
            None,
        );
        let dd_cmd = Self::wrap_for_root(&dd_cmd, root_type);

        let mut guard = DeviceTempGuard::new(serial, temp.clone(), root_type, force_usb);

        emit_progress(app, FlashStage::Sending, &format!("Dumping {progress_label}..."), None);
        info!("Dump dd: {}", dd_cmd);
        let dd_output = adb_shell(serial, &dd_cmd, Some(Duration::from_secs(30)), force_usb)?.stdout;
        info!("dd output: {}", dd_output.trim());

        emit_progress(app, FlashStage::Sending, &format!("Reading {progress_label}..."), Some(0.0));
        sync_pull(serial, &temp, local_path, app, force_usb)?;

        let rm_cmd = Self::wrap_for_root(&format!("rm {temp}"), root_type);
        if adb_shell(serial, &rm_cmd, ADB_SHELL_TIMEOUT, force_usb).is_ok() {
            guard.disarm();
        }

        Ok(())
    }

    /// Dump a raw block device region to a local file. BLOCKING.
    #[allow(clippy::too_many_arguments)]
    pub fn run_dump_image(
        serial: &str,
        device: &str,
        offset_bytes: Option<u64>,
        size_bytes: Option<u64>,
        local_path: &Path,
        root_type: &RootType,
        temp_dir: &str,
        app: &AppHandle,
        force_usb: bool,
    ) -> Result<(), FlashError> {
        Self::validate_shell_path(device)?;
        Self::validate_shell_path(temp_dir)?;

        // Shell V2: stream dd directly to local file
        if shell_v2_supported(serial, force_usb) {
            let dd_cmd = if let (Some(off), Some(sz)) = (offset_bytes, size_bytes) {
                format!("dd if={device} bs=512 skip={} count={}", off / 512, sz / 512)
            } else {
                format!("dd if={device} bs=65536")
            };
            let dd_cmd = Self::wrap_for_root(&dd_cmd, root_type);

            emit_progress(app, FlashStage::Sending, "Reading image...", Some(0.0));
            info!("Dump V2 stream: {}", dd_cmd);

            let tmp_path = local_path.with_extension("img.tmp");
            let mut file = std::fs::File::create(&tmp_path).map_err(FlashError::Io)?;

            let exit_code = match adb_shell_stream(serial, &dd_cmd, &mut file, app, size_bytes, "image", force_usb) {
                Ok(code) => code,
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp_path);
                    if e != FlashError::DeviceDisconnected {
                        emit_progress(app, FlashStage::Error, &format!("Dump failed: {e}"), None);
                    }
                    return Err(e);
                }
            };

            if exit_code != 0 {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(FlashError::Protocol(format!("dd failed with exit code {exit_code}")));
            }

            std::fs::rename(&tmp_path, local_path).map_err(FlashError::Io)?;
            emit_progress(app, FlashStage::Complete, "Dump complete!", Some(100.0));
            return Ok(());
        }

        // Shell V1 fallback
        Self::cleanup_device_temps(serial, temp_dir, root_type, force_usb);

        let temp = Self::temp_path_in(temp_dir, "raw");
        let dd_cmd = Self::build_dd_command(device, &temp, offset_bytes, size_bytes);
        let dd_cmd = Self::wrap_for_root(&dd_cmd, root_type);

        let mut guard = DeviceTempGuard::new(serial, temp.clone(), root_type, force_usb);

        emit_progress(app, FlashStage::Sending, "Dumping image...", None);
        info!("Dump dd: {}", dd_cmd);
        let dd_output = adb_shell(serial, &dd_cmd, Some(Duration::from_secs(30)), force_usb)?.stdout;
        info!("dd output: {}", dd_output.trim());

        emit_progress(app, FlashStage::Sending, "Reading image...", Some(0.0));
        sync_pull(serial, &temp, local_path, app, force_usb)?;

        let rm_cmd = Self::wrap_for_root(&format!("rm {temp}"), root_type);
        if adb_shell(serial, &rm_cmd, ADB_SHELL_TIMEOUT, force_usb).is_ok() {
            guard.disarm();
        }

        emit_progress(app, FlashStage::Complete, "Dump complete!", Some(100.0));
        Ok(())
    }

    /// Remove orphaned .rusty_write_*.img temp files from device.
    fn cleanup_write_temps(serial: &str, temp_dir: &str, root_type: &RootType, force_usb: bool) {
        let ls_cmd = format!("ls {temp_dir}/.rusty_write_*.img 2>/dev/null");
        let ls_cmd = Self::wrap_for_root(&ls_cmd, root_type);

        let files = match adb_shell(serial, &ls_cmd, ADB_SHELL_TIMEOUT, force_usb) {
            Ok(result) => result.stdout
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| l.contains(".rusty_write_") && l.ends_with(".img"))
                .collect::<Vec<_>>(),
            Err(_) => return,
        };

        for f in &files {
            let rm_cmd = Self::wrap_for_root(&format!("rm {f}"), root_type);
            let _ = adb_shell(serial, &rm_cmd, ADB_SHELL_TIMEOUT, force_usb);
            info!("Cleaned orphaned write temp: {f}");
        }
    }

    /// Write a local .img file to a device partition. BLOCKING.
    /// V2 path: stream file through Shell V2 stdin to dd.
    /// V1 path: push file to device temp, then dd to partition.
    #[allow(clippy::too_many_arguments)]
    pub fn run_write_partition(
        serial: &str,
        partition: &str,
        local_path: &Path,
        root_type: &RootType,
        temp_dir: &str,
        app: &AppHandle,
        progress_label: &str,
        force_usb: bool,
    ) -> Result<(), FlashError> {
        Self::validate_partition_name(partition)?;
        Self::validate_shell_path(temp_dir)?;

        let metadata = std::fs::metadata(local_path).map_err(FlashError::Io)?;
        let file_size = metadata.len();
        if file_size == 0 {
            return Err(FlashError::Validation("Image file is empty".into()));
        }

        // Check file size vs partition size to prevent writing past partition boundary
        let size_cmd = format!("blockdev --getsize64 /dev/block/by-name/{partition}");
        let size_cmd = Self::wrap_for_root(&size_cmd, root_type);
        if let Ok(result) = adb_shell(serial, &size_cmd, super::ADB_SHELL_TIMEOUT, force_usb) {
            if let Ok(partition_size) = result.stdout.trim().parse::<u64>() {
                if file_size > partition_size {
                    return Err(FlashError::Validation(format!(
                        "Image ({}) exceeds partition size ({})",
                        Self::format_size(file_size),
                        Self::format_size(partition_size),
                    )));
                }
            }
        }

        // Shell V2: stream file directly to dd stdin
        if shell_v2_supported(serial, force_usb) {
            let dd_cmd = format!("dd of=/dev/block/by-name/{partition} bs=65536");
            let dd_cmd = Self::wrap_for_root(&dd_cmd, root_type);

            emit_progress(app, FlashStage::Sending, &format!("Writing {progress_label}..."), Some(0.0));
            info!("Write V2 stream: {}", dd_cmd);

            let mut file = std::fs::File::open(local_path).map_err(FlashError::Io)?;
            let exit_code = adb_shell_stream_stdin(
                serial, &dd_cmd, &mut file, file_size, app, progress_label, force_usb,
            )?;

            if exit_code != 0 {
                emit_progress(app, FlashStage::Error, &format!("dd failed with exit code {exit_code}"), None);
                return Err(FlashError::Protocol(format!("dd failed with exit code {exit_code}")));
            }

            emit_progress(app, FlashStage::Complete, &format!("{progress_label} written!"), Some(100.0));
            return Ok(());
        }

        // Shell V1 fallback: push .img to device temp, then dd to partition
        Self::cleanup_write_temps(serial, temp_dir, root_type, force_usb);

        let temp = Self::write_temp_path_in(temp_dir, partition);
        let mut guard = DeviceTempGuard::new(serial, temp.clone(), root_type, force_usb);

        emit_progress(app, FlashStage::Sending, &format!("Pushing {progress_label}..."), Some(0.0));
        info!("Write V1: pushing {} to {}", local_path.display(), temp);
        sync_push(serial, local_path, &temp, app, force_usb)?;

        let dd_cmd = Self::build_dd_command(&temp, &format!("/dev/block/by-name/{partition}"), None, None);
        let dd_cmd = Self::wrap_for_root(&dd_cmd, root_type);

        emit_progress(app, FlashStage::Sending, &format!("Writing {progress_label} to partition..."), None);
        info!("Write dd: {}", dd_cmd);
        let dd_result = adb_shell(serial, &dd_cmd, None, force_usb)?;
        info!("dd output: {}", dd_result.stdout.trim());

        // V1 has no exit codes — check stderr/stdout for known dd error patterns
        let combined = format!("{} {}", dd_result.stdout, dd_result.stderr);
        let combined_lower = combined.to_lowercase();
        if combined_lower.contains("no space left") || combined_lower.contains("permission denied")
            || combined_lower.contains("read-only file system") || combined_lower.contains("i/o error")
        {
            emit_progress(app, FlashStage::Error, &format!("dd failed: {}", combined.trim()), None);
            return Err(FlashError::Protocol(format!("dd failed: {}", combined.trim())));
        }

        // Sync to ensure write is flushed to disk
        let sync_cmd = Self::wrap_for_root("sync", root_type);
        let _ = adb_shell(serial, &sync_cmd, ADB_SHELL_TIMEOUT, force_usb);

        let rm_cmd = Self::wrap_for_root(&format!("rm {temp}"), root_type);
        if adb_shell(serial, &rm_cmd, ADB_SHELL_TIMEOUT, force_usb).is_ok() {
            guard.disarm();
        }

        emit_progress(app, FlashStage::Complete, &format!("{progress_label} written!"), Some(100.0));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_partition_name_safe() {
        assert!(AdbProtocol::validate_partition_name("boot").is_ok());
        assert!(AdbProtocol::validate_partition_name("system_a").is_ok());
        assert!(AdbProtocol::validate_partition_name("boot-backup.1").is_ok());
    }

    #[test]
    fn test_validate_partition_name_unsafe() {
        assert!(AdbProtocol::validate_partition_name("").is_err());
        assert!(AdbProtocol::validate_partition_name("test;rm -rf /").is_err());
        assert!(AdbProtocol::validate_partition_name("$(whoami)").is_err());
        assert!(AdbProtocol::validate_partition_name("test`id`").is_err());
        assert!(AdbProtocol::validate_partition_name("a b").is_err());
    }

    #[test]
    fn test_build_dd_command_partition() {
        let cmd = AdbProtocol::build_dd_command(
            "/dev/block/by-name/boot",
            "/sdcard/.rusty_dump_boot.img",
            None, None,
        );
        assert_eq!(cmd, "dd if=/dev/block/by-name/boot of=/sdcard/.rusty_dump_boot.img bs=4096");
    }

    #[test]
    fn test_build_dd_command_with_offset_and_size() {
        let cmd = AdbProtocol::build_dd_command(
            "/dev/block/sda",
            "/sdcard/.rusty_dump_raw.img",
            Some(8192),
            Some(16384),
        );
        assert_eq!(cmd, "dd if=/dev/block/sda of=/sdcard/.rusty_dump_raw.img bs=4096 skip=2 count=4");
    }

    #[test]
    fn test_build_dd_command_offset_rounds_down() {
        let cmd = AdbProtocol::build_dd_command(
            "/dev/block/sda",
            "/sdcard/.rusty_dump_raw.img",
            Some(5000), Some(4096),
        );
        assert_eq!(cmd, "dd if=/dev/block/sda of=/sdcard/.rusty_dump_raw.img bs=4096 skip=1 count=1");
    }

    #[test]
    fn test_build_dd_command_size_rounds_up() {
        let cmd = AdbProtocol::build_dd_command(
            "/dev/block/sda",
            "/sdcard/.rusty_dump_raw.img",
            None, Some(5000),
        );
        assert_eq!(cmd, "dd if=/dev/block/sda of=/sdcard/.rusty_dump_raw.img bs=4096 count=2");
    }

    #[test]
    fn test_temp_filename() {
        assert_eq!(AdbProtocol::temp_path("boot"), "/sdcard/.rusty_dump_boot.img");
        assert_eq!(AdbProtocol::temp_path("system_a"), "/sdcard/.rusty_dump_system_a.img");
    }

    #[test]
    fn test_temp_path_in() {
        assert_eq!(
            AdbProtocol::temp_path_in("/data/local/tmp", "boot"),
            "/data/local/tmp/.rusty_dump_boot.img"
        );
        assert_eq!(
            AdbProtocol::temp_path_in("/sdcard", "system_a"),
            "/sdcard/.rusty_dump_system_a.img"
        );
    }

    #[test]
    fn test_parse_batch_partition_output() {
        let output = "boot 67108864\nrecovery 33554432\nsystem 4294967296\n";
        let parts = AdbProtocol::parse_batch_partition_output(output);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].name, "boot");
        assert_eq!(parts[0].size_bytes, Some(67108864));
        assert_eq!(parts[0].size_display, "64.0 MB");
        assert_eq!(parts[2].name, "system");
        assert_eq!(parts[2].size_bytes, Some(4294967296));
    }

    #[test]
    fn test_parse_batch_partition_output_with_errors() {
        let output = "boot 67108864\nbad_size nope\nempty_line \n\nrecovery 0\n";
        let parts = AdbProtocol::parse_batch_partition_output(output);
        assert!(parts.iter().any(|p| p.name == "boot" && p.size_bytes == Some(67108864)));
        assert!(parts.iter().any(|p| p.name == "bad_size" && p.size_bytes.is_none()));
    }

    #[test]
    fn test_parse_batch_partition_output_skips_unsafe_names() {
        let output = "boot 67108864\n$(whoami) 1024\nnormal_name 2048\n";
        let parts = AdbProtocol::parse_batch_partition_output(output);
        assert!(parts.iter().all(|p| p.name != "$(whoami)"));
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn test_write_temp_path_in() {
        assert_eq!(
            AdbProtocol::write_temp_path_in("/data/local/tmp", "boot"),
            "/data/local/tmp/.rusty_write_boot.img"
        );
        assert_eq!(
            AdbProtocol::write_temp_path_in("/sdcard", "system_a"),
            "/sdcard/.rusty_write_system_a.img"
        );
    }

    #[test]
    fn test_build_dd_command_write_partition() {
        let cmd = AdbProtocol::build_dd_command(
            "/sdcard/.rusty_write_boot.img",
            "/dev/block/by-name/boot",
            None, None,
        );
        assert_eq!(cmd, "dd if=/sdcard/.rusty_write_boot.img of=/dev/block/by-name/boot bs=4096");
    }
}
