use tracing::debug;

use crate::error::FlashError;
use crate::types::{AdbState, DeviceInfo, ProtocolType};

// ---------------------------------------------------------------------------
// Android USB interface class constants (AOSP)
// ---------------------------------------------------------------------------

const ANDROID_CLASS: u8 = 0xFF;    // Vendor-specific
const ANDROID_SUBCLASS: u8 = 0x42; // Android
const ADB_PROTOCOL: u8 = 0x01;     // ADB
const FASTBOOT_PROTOCOL: u8 = 0x03; // Fastboot

/// VID/PID table — ONLY for protocols that lack a standard USB interface class.
/// ADB and Fastboot are detected by interface descriptor, NOT by this table.
const KNOWN_DEVICES: &[(u16, u16, ProtocolType)] = &[
    // EDL (Qualcomm) — no standard USB class, must use VID/PID
    (0x05C6, 0x9008, ProtocolType::Edl),
    // MTK BROM — no standard USB class, must use VID/PID
    (0x0E8D, 0x0003, ProtocolType::MtkBrom),
];

/// Match a VID/PID pair against the known device table (EDL, MTK only).
pub fn identify_protocol(vendor_id: u16, product_id: u16) -> Option<ProtocolType> {
    KNOWN_DEVICES
        .iter()
        .find(|(vid, pid, _)| *vid == vendor_id && *pid == product_id)
        .map(|(_, _, protocol)| protocol.clone())
}

/// Identify protocol from USB interface descriptor — the definitive signal.
///
/// Class 0xFF, Subclass 0x42, Protocol 0x01 → ADB
/// Class 0xFF, Subclass 0x42, Protocol 0x03 → Fastboot
fn identify_by_interface(class: u8, subclass: u8, protocol: u8) -> Option<ProtocolType> {
    if class == ANDROID_CLASS && subclass == ANDROID_SUBCLASS {
        match protocol {
            ADB_PROTOCOL => Some(ProtocolType::Adb),
            FASTBOOT_PROTOCOL => Some(ProtocolType::Fastboot),
            _ => None,
        }
    } else {
        None
    }
}

/// Scan all connected USB devices and return recognized ones.
///
/// Detection priority:
/// 1. USB interface descriptor (ADB/Fastboot) — definitive, no VID/PID needed
/// 2. VID/PID table (EDL/MTK) — for protocols without standard USB classes
///
/// For interface-based detection, opens the device and scans the active
/// configuration descriptor. Also extracts bulk endpoint addresses for
/// ADB devices.
pub fn scan_devices() -> Result<Vec<DeviceInfo>, FlashError> {
    let mut found = Vec::new();

    let devices = nusb::list_devices()
        .map_err(|e| FlashError::Usb(e.to_string()))?;

    for dev in devices {
        let vid = dev.vendor_id();
        let pid = dev.product_id();
        let serial = dev.serial_number().map(|s: &str| s.to_string());
        let manufacturer = dev.manufacturer_string().map(|s: &str| s.to_string());
        let product = dev.product_string().map(|s: &str| s.to_string());

        // 1. Try interface descriptor detection (ADB / Fastboot).
        //    Check nusb's pre-enumerated interface list first (no device open needed).
        let mut protocol_from_intf = None;
        for intf in dev.interfaces() {
            if let Some(proto) = identify_by_interface(intf.class(), intf.subclass(), intf.protocol()) {
                protocol_from_intf = Some(proto);
                break;
            }
        }

        // If interface list is empty (common on Windows with non-usbccgp composites),
        // try opening the device and scanning the config descriptor.
        if protocol_from_intf.is_none() {
            if let Ok(device) = dev.open() {
                if let Ok(config) = device.active_configuration() {
                    for alt_setting in config.interface_alt_settings() {
                        if let Some(proto) = identify_by_interface(
                            alt_setting.class(),
                            alt_setting.subclass(),
                            alt_setting.protocol(),
                        ) {
                            protocol_from_intf = Some(proto);
                            break;
                        }
                    }
                }
            }
        }

        if let Some(protocol) = protocol_from_intf {
            let is_adb = matches!(protocol, ProtocolType::Adb);
            debug!(
                "Detected {:?} via interface descriptor: {:04x}:{:04x} {:?}",
                protocol, vid, pid, serial
            );
            found.push(DeviceInfo {
                vendor_id: vid,
                product_id: pid,
                serial,
                manufacturer,
                product,
                protocol,
                adb_state: if is_adb { Some(AdbState::Normal) } else { None },
            });
            continue;
        }

        // 2. Fallback: VID/PID table (EDL, MTK BROM).
        if let Some(protocol) = identify_protocol(vid, pid) {
            debug!(
                "Detected {:?} via VID/PID: {:04x}:{:04x} {:?}",
                protocol, vid, pid, serial
            );
            found.push(DeviceInfo {
                vendor_id: vid,
                product_id: pid,
                serial,
                manufacturer,
                product,
                protocol,
                adb_state: None,
            });
        }
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identify_by_interface_adb() {
        assert_eq!(identify_by_interface(0xFF, 0x42, 0x01), Some(ProtocolType::Adb));
    }

    #[test]
    fn test_identify_by_interface_fastboot() {
        assert_eq!(identify_by_interface(0xFF, 0x42, 0x03), Some(ProtocolType::Fastboot));
    }

    #[test]
    fn test_identify_by_interface_unknown_protocol() {
        assert_eq!(identify_by_interface(0xFF, 0x42, 0x05), None);
    }

    #[test]
    fn test_identify_by_interface_wrong_class() {
        assert_eq!(identify_by_interface(0x00, 0x42, 0x01), None);
    }

    #[test]
    fn test_identify_by_interface_wrong_subclass() {
        assert_eq!(identify_by_interface(0xFF, 0x00, 0x01), None);
    }

    #[test]
    fn test_identify_qualcomm_edl() {
        assert_eq!(identify_protocol(0x05C6, 0x9008), Some(ProtocolType::Edl));
    }

    #[test]
    fn test_identify_mtk_brom() {
        assert_eq!(identify_protocol(0x0E8D, 0x0003), Some(ProtocolType::MtkBrom));
    }

    #[test]
    fn test_identify_unknown_vid_pid() {
        assert_eq!(identify_protocol(0xFFFF, 0xFFFF), None);
    }

    #[test]
    fn test_old_fastboot_vid_pid_no_longer_matches() {
        // These used to be in the VID/PID table — now detected by interface descriptor only
        assert_eq!(identify_protocol(0x18D1, 0xD00D), None);
        assert_eq!(identify_protocol(0x18D1, 0x4EE0), None);
    }

}
