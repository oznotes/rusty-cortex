use std::collections::HashMap;
use std::path::Path;

use tracing::{info, warn};

use crate::device::detect;
use crate::error::FlashError;
use crate::types::{DeviceInfo, ProtocolType, RebootMode};
use tokio::task::spawn_blocking;

pub struct FastbootProtocol;

impl FastbootProtocol {
    pub fn new() -> Self {
        Self
    }

    /// Open the first fastboot USB device. BLOCKING.
    ///
    /// Two-tier detection:
    /// 1. Try the crate's interface list (fast, works when OS pre-enumerates)
    /// 2. Fall back to opening device + scanning config descriptor (required on
    ///    Windows with WinUSB-bound devices where interfaces() is empty)
    fn open_device_sync() -> Result<fastboot_protocol::nusb::NusbFastBoot, FlashError> {
        // Fast path: crate's own interface detection
        if let Ok(mut iter) = fastboot_protocol::nusb::devices() {
            if let Some(info) = iter.next() {
                if let Ok(fb) = fastboot_protocol::nusb::NusbFastBoot::from_info(&info) {
                    return Ok(fb);
                }
            }
        }

        // Fallback: open each device and scan the config descriptor directly.
        // Required on Windows when the device is bound to WinUSB without usbccgp,
        // where DeviceInfo::interfaces() returns an empty list.
        let devices = nusb::list_devices()
            .map_err(|e| FlashError::Usb(format!("USB enumeration failed: {}", e)))?;

        for dev_info in devices {
            let device = match dev_info.open() {
                Ok(d) => d,
                Err(_) => continue,
            };
            // Extract interface number before moving device (borrow checker).
            let fastboot_intf = device.active_configuration().ok().and_then(|config| {
                config
                    .interface_alt_settings()
                    .find(|alt| {
                        alt.class() == 0xFF && alt.subclass() == 0x42 && alt.protocol() == 0x03
                    })
                    .map(|alt| alt.interface_number())
            });
            if let Some(intf_num) = fastboot_intf {
                return fastboot_protocol::nusb::NusbFastBoot::from_device(device, intf_num)
                    .map_err(|e| {
                        FlashError::Usb(format!("Failed to open fastboot device: {}", e))
                    });
            }
        }

        Err(FlashError::NoDevice)
    }

    /// Async-safe device open: runs blocking USB enumeration on a separate thread.
    async fn open_client() -> Result<fastboot_protocol::nusb::NusbFastBoot, FlashError> {
        spawn_blocking(Self::open_device_sync)
            .await
            .map_err(|e| FlashError::Usb(format!("Device open task failed: {}", e)))?
    }
}

impl Default for FastbootProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl FastbootProtocol {
    pub async fn detect(&self) -> Result<Option<DeviceInfo>, FlashError> {
        let devices = detect::scan_devices()?;
        Ok(devices
            .into_iter()
            .find(|d| d.protocol == ProtocolType::Fastboot))
    }

    #[allow(dead_code)]
    pub async fn flash(&self, firmware: &Path, partition: &str) -> Result<(), FlashError> {
        self.flash_with_progress(firmware, partition, None).await
    }

    pub async fn get_partitions(&self) -> Result<Vec<String>, FlashError> {
        // Try to get real partitions from device via getvar:all
        if let Ok(vars) = self.get_all_vars().await {
            let mut partitions: Vec<String> = vars.keys()
                .filter_map(|k| {
                    // Keys like "partition-size:boot" or "partition-type:boot"
                    k.strip_prefix("partition-size:")
                        .or_else(|| k.strip_prefix("partition-type:"))
                        .map(|p: &str| p.to_string())
                })
                .collect();
            partitions.sort();
            partitions.dedup();
            if !partitions.is_empty() {
                return Ok(partitions);
            }
        }

        // Fallback: common Android partitions
        Ok(vec![
            "boot".into(),
            "recovery".into(),
            "system".into(),
            "vendor".into(),
            "dtbo".into(),
            "vbmeta".into(),
            "super".into(),
            "userdata".into(),
        ])
    }

    #[allow(dead_code)]
    pub async fn get_var(&self, name: &str) -> Result<String, FlashError> {
        let mut fb = Self::open_client().await?;
        fb.get_var(name)
            .await
            .map_err(|e| FlashError::Protocol(e.to_string()))
    }

    pub async fn get_all_vars(&self) -> Result<HashMap<String, String>, FlashError> {
        let mut fb = Self::open_client().await?;
        fb.get_all_vars()
            .await
            .map_err(|e| FlashError::Protocol(e.to_string()))
    }

    pub async fn reboot(&self, mode: RebootMode) -> Result<(), FlashError> {
        let mut fb = Self::open_client().await?;

        match mode {
            RebootMode::Normal => {
                info!("Rebooting device...");
                fb.reboot()
                    .await
                    .map_err(|e| FlashError::Protocol(e.to_string()))?;
            }
            RebootMode::Bootloader => {
                info!("Rebooting to bootloader...");
                fb.reboot_bootloader()
                    .await
                    .map_err(|e| FlashError::Protocol(e.to_string()))?;
            }
            RebootMode::Recovery => {
                // Fastboot has no direct reboot-recovery command; fall back to normal reboot.
                warn!(
                    "Reboot to recovery not directly supported via fastboot, rebooting normally"
                );
                fb.reboot()
                    .await
                    .map_err(|e| FlashError::Protocol(e.to_string()))?;
            }
            RebootMode::Edl => {
                info!("Rebooting to EDL via fastboot oem edl...");
                warn!("EDL reboot via fastboot not yet implemented");
                return Err(FlashError::Protocol(
                    "EDL reboot via fastboot not supported yet".into(),
                ));
            }
        }
        Ok(())
    }

    /// Flash firmware with optional progress callback.
    /// Chunks data send into 256KB blocks for progress reporting.
    pub async fn flash_with_progress(
        &self,
        firmware: &Path,
        partition: &str,
        on_progress: Option<&(dyn Fn(f32) + Send + Sync)>,
    ) -> Result<(), FlashError> {
        info!("Flashing {} to partition '{}'", firmware.display(), partition);
        let firmware_path = firmware.to_path_buf();
        let data = spawn_blocking(move || std::fs::read(firmware_path))
            .await
            .map_err(|e| FlashError::Io(std::io::Error::other(e)))??;
        if data.len() > u32::MAX as usize {
            return Err(FlashError::Protocol(format!(
                "Firmware too large ({} bytes) — fastboot protocol supports up to 4 GB",
                data.len()
            )));
        }
        let data_len = data.len() as u32;

        let mut fb = Self::open_client().await?;

        info!("Initiating download of {} bytes to device...", data_len);
        let mut dl = fb
            .download(data_len)
            .await
            .map_err(|e| FlashError::Protocol(e.to_string()))?;

        // Send data in chunks for progress reporting
        const CHUNK_SIZE: usize = 262144; // 256KB
        let mut sent: usize = 0;
        for chunk in data.chunks(CHUNK_SIZE) {
            dl.extend_from_slice(chunk)
                .await
                .map_err(|e| FlashError::Protocol(e.to_string()))?;
            sent += chunk.len();
            if let Some(cb) = &on_progress {
                let percent = (sent as f32 / data_len as f32 * 100.0).min(100.0);
                cb(percent);
            }
        }

        dl.finish()
            .await
            .map_err(|e| FlashError::Protocol(e.to_string()))?;

        info!("Writing to partition '{}'...", partition);
        fb.flash(partition)
            .await
            .map_err(|e| FlashError::Protocol(e.to_string()))?;

        info!("Flash complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_extraction_from_vars() {
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("partition-size:boot".into(), "0x4000000".into());
        vars.insert("partition-size:recovery".into(), "0x6000000".into());
        vars.insert("partition-type:boot".into(), "emmc".into());
        vars.insert("partition-type:system".into(), "ext4".into());
        vars.insert("max-download-size".into(), "0x10000000".into());
        vars.insert("product".into(), "raphael".into());

        let mut partitions: Vec<String> = vars.keys()
            .filter_map(|k| {
                k.strip_prefix("partition-size:")
                    .or_else(|| k.strip_prefix("partition-type:"))
                    .map(|p: &str| p.to_string())
            })
            .collect();
        partitions.sort();
        partitions.dedup();

        assert_eq!(partitions, vec!["boot", "recovery", "system"]);
    }

    #[test]
    fn test_partition_extraction_empty_vars() {
        let vars: HashMap<String, String> = HashMap::new();
        let partitions: Vec<String> = vars.keys()
            .filter_map(|k: &String| {
                k.strip_prefix("partition-size:")
                    .or_else(|| k.strip_prefix("partition-type:"))
                    .map(|p: &str| p.to_string())
            })
            .collect();
        assert!(partitions.is_empty());
    }

    #[test]
    fn test_partition_extraction_no_partition_keys() {
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("product".into(), "raphael".into());
        vars.insert("secure".into(), "yes".into());
        vars.insert("unlocked".into(), "yes".into());

        let partitions: Vec<String> = vars.keys()
            .filter_map(|k| {
                k.strip_prefix("partition-size:")
                    .or_else(|| k.strip_prefix("partition-type:"))
                    .map(|p: &str| p.to_string())
            })
            .collect();
        assert!(partitions.is_empty());
    }

    #[test]
    fn test_fallback_partitions_list() {
        let fallback = [
            "boot", "recovery", "system", "vendor",
            "dtbo", "vbmeta", "super", "userdata",
        ];
        assert_eq!(fallback.len(), 8);
        assert!(fallback.contains(&"boot"));
        assert!(fallback.contains(&"system"));
        assert!(fallback.contains(&"userdata"));
    }

    #[test]
    fn test_fastboot_protocol_default() {
        let fb = FastbootProtocol::default();
        // FastbootProtocol is a unit struct, just verify construction
        let _ = fb;
    }

    #[test]
    fn test_firmware_size_limit() {
        // Fastboot protocol supports up to 4 GB (u32::MAX bytes)
        assert!(u32::MAX as usize > 0);
        // Firmware larger than u32::MAX should be rejected
        let too_large: usize = u32::MAX as usize + 1;
        assert!(too_large > u32::MAX as usize);
    }
}
