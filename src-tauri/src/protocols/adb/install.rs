//! ADB APK installation.

use std::path::Path;
use std::time::Duration;

use tracing::info;
use tauri::AppHandle;

use crate::error::FlashError;
use crate::types::{FlashStage, RootType};
use super::{AdbProtocol, ADB_SHELL_TIMEOUT, emit_progress};
use super::shell::adb_shell;
use super::sync::sync_push;
use super::dump::DeviceTempGuard;

impl AdbProtocol {
    /// Build install flag list from boolean options.
    pub fn build_install_flags(replace: bool, downgrade: bool, grant_all: bool) -> Vec<&'static str> {
        let mut flags = Vec::new();
        if replace { flags.push("-r"); }
        if downgrade { flags.push("-d"); }
        if grant_all { flags.push("-g"); }
        flags
    }

    /// Build the `pm install` command string.
    pub fn build_pm_install_command(device_path: &str, flags: &[&str]) -> String {
        if flags.is_empty() {
            format!("pm install {device_path}")
        } else {
            format!("pm install {} {device_path}", flags.join(" "))
        }
    }

    /// Install an APK on the device. BLOCKING.
    pub fn run_install(
        serial: &str,
        apk_path: &Path,
        replace: bool,
        downgrade: bool,
        grant_all: bool,
        app: &AppHandle,
        force_usb: bool,
    ) -> Result<(), FlashError> {
        let filename = apk_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| FlashError::Validation("Invalid APK filename".into()))?;

        // Validate filename — prevent shell injection
        if !filename.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
            return Err(FlashError::Validation(format!(
                "APK filename contains unsafe characters: {}. Rename the file to use only letters, numbers, underscores, hyphens, and dots.", filename
            )));
        }

        let device_tmp = format!("/data/local/tmp/{filename}");
        let mut guard = DeviceTempGuard::new(serial, device_tmp.clone(), &RootType::Adb, force_usb);

        // Phase 1: Push APK to device temp
        emit_progress(app, FlashStage::Sending, &format!("Pushing {filename}..."), Some(0.0));
        sync_push(serial, apk_path, &device_tmp, app, force_usb)?;

        // Phase 2: Install via pm
        emit_progress(app, FlashStage::Flashing, &format!("Installing {filename}..."), Some(80.0));
        let flags = Self::build_install_flags(replace, downgrade, grant_all);
        let pm_cmd = Self::build_pm_install_command(&device_tmp, &flags);
        info!("Install: {}", pm_cmd);
        let pm_result = adb_shell(serial, &pm_cmd, Some(Duration::from_secs(300)), force_usb)?;
        info!("pm output: {}", pm_result.stdout.trim());

        // Check for pm install failure.
        // Shell V2: use exit code (0 = success). V1: check for "Failure" keyword.
        // AOSP pm outputs "Success"/"Failure [CODE]" in English regardless of locale,
        // but checking for failure is more robust than checking for success.
        let install_failed = if let Some(code) = pm_result.exit_code {
            code != 0
        } else {
            pm_result.stdout.contains("Failure") || !pm_result.stdout.contains("Success")
        };
        if install_failed {
            let _ = adb_shell(serial, &format!("rm {device_tmp}"), ADB_SHELL_TIMEOUT, force_usb);
            guard.disarm();
            return Err(FlashError::Protocol(format!("Install failed: {}", pm_result.stdout.trim())));
        }

        // Phase 3: Cleanup temp APK
        let _ = adb_shell(serial, &format!("rm {device_tmp}"), ADB_SHELL_TIMEOUT, force_usb);
        guard.disarm();

        emit_progress(app, FlashStage::Complete, &format!("{filename} installed!"), Some(100.0));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_install_flags() {
        assert_eq!(AdbProtocol::build_install_flags(true, false, false), vec!["-r"]);
        assert_eq!(AdbProtocol::build_install_flags(true, true, true), vec!["-r", "-d", "-g"]);
        assert_eq!(AdbProtocol::build_install_flags(false, false, false), Vec::<&str>::new());
    }

    #[test]
    fn test_build_pm_install_command() {
        assert_eq!(
            AdbProtocol::build_pm_install_command("/data/local/tmp/app.apk", &["-r", "-g"]),
            "pm install -r -g /data/local/tmp/app.apk"
        );
        assert_eq!(
            AdbProtocol::build_pm_install_command("/data/local/tmp/test.apk", &[]),
            "pm install /data/local/tmp/test.apk"
        );
    }
}
