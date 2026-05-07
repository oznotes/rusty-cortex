use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ProtocolType {
    Fastboot,
    Adb,
    Edl,
    MtkBrom,
}

impl std::fmt::Display for ProtocolType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolType::Fastboot => write!(f, "Fastboot"),
            ProtocolType::Adb => write!(f, "ADB"),
            ProtocolType::Edl => write!(f, "EDL"),
            ProtocolType::MtkBrom => write!(f, "MTK BROM"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AdbState {
    Normal,
    Recovery,
    Sideload,
}

impl std::fmt::Display for AdbState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdbState::Normal => write!(f, "Normal"),
            AdbState::Recovery => write!(f, "Recovery"),
            AdbState::Sideload => write!(f, "Sideload"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub protocol: ProtocolType,
    #[serde(default)]
    pub adb_state: Option<AdbState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FlashStage {
    Idle,
    Validating,
    Sending,
    Flashing,
    Complete,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlashProgress {
    pub stage: FlashStage,
    pub message: String,
    pub percent: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RebootMode {
    Normal,
    Bootloader,
    Recovery,
    Edl,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RootType {
    None,
    Adb,
    Su,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootStatus {
    pub root_type: RootType,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionInfo {
    pub name: String,
    pub size_bytes: Option<u64>,
    pub size_display: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpListResult {
    pub partitions: Vec<PartitionInfo>,
    pub temp_dir: String,
    pub free_bytes: Option<u64>,
    pub supports_shell_v2: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceHealth {
    pub battery_level: Option<u32>,
    pub battery_health: Option<String>,
    pub battery_temp: Option<f32>,
    pub storage_used_gb: Option<f32>,
    pub storage_total_gb: Option<f32>,
    pub ram_used_gb: Option<f32>,
    pub ram_total_gb: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdlDeviceInfo {
    pub serial: Option<String>,
    pub hw_id: Option<String>,
    pub pk_hash: Option<String>,
    pub storage_type: Option<String>,
    pub sector_size: Option<u32>,
    pub num_luns: Option<u8>,
    /// Device already in Firehose mode from a previous session (Sahara skipped).
    #[serde(default)]
    pub firehose_active: bool,
    /// Human-readable chipset name resolved from HWID (e.g., "SM8150 (SDM855)").
    #[serde(default)]
    pub chipset: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdlPartitionEntry {
    pub name: String,
    pub start_sector: u64,
    pub num_sectors: u64,
    pub size_bytes: u64,
    pub lun: u8,
    pub type_guid: String,
    pub unique_guid: String,
    pub attributes: u64,
    pub category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawprogramDiscovery {
    pub rawprogram_path: String,
    pub patch_path: Option<String>,
    pub lun_hint: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchFlashResult {
    pub programmed: Vec<String>,
    pub erased: Vec<String>,
    pub patched: usize,
    pub errors: Vec<String>,
    pub verified: Vec<(String, bool)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgrammerEntry {
    pub programmer_path: String,
    pub programmer_name: String,
    pub device_serial: Option<String>,
    pub storage_type: Option<String>,
    pub last_used: String,
    pub use_count: u32,
    #[serde(default)]
    pub file_exists: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MatchLevel {
    DbExact,
    BinaryVerified,
    FilenameMatch,
    #[default]
    Unknown,
    DbOtherDevice,
}

/// Hash algorithm used for PKHash in Qualcomm programmer certificates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum HashAlgorithm {
    Sha256,
    Sha384,
}

impl std::fmt::Display for HashAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HashAlgorithm::Sha256 => write!(f, "SHA-256"),
            HashAlgorithm::Sha384 => write!(f, "SHA-384"),
        }
    }
}

/// Identity extracted from a programmer binary's certificate chain.
/// None if parsing fails (corrupt file, non-Qualcomm binary, no cert chain).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgrammerIdentity {
    /// Full HWID as hex string in bkerler format (e.g., "000a50e100720000")
    pub hw_id: String,
    /// PKHash: single hash (64 hex chars for SHA-256, 96 for SHA-384)
    pub pk_hash: String,
    /// Hash algorithm detected from the root certificate's signature algorithm OID
    pub hash_algorithm: HashAlgorithm,
    /// Qualcomm MSM chip ID (e.g., 0x0A50E1 = SDM855)
    pub msm_id: u32,
    /// OEM identifier
    pub oem_id: u16,
    /// Model identifier
    pub model_id: u16,
    /// Human-readable chipset name if known (e.g., "SM8150 (SDM855)")
    pub chipset: Option<String>,
    /// True if HWID was extracted from filename (bkerler convention), not binary metadata
    #[serde(default)]
    pub hwid_from_filename: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgrammerCandidate {
    pub name: String,
    pub path: String,
    pub valid: bool,
    pub size_bytes: u64,
    #[serde(default)]
    pub match_level: MatchLevel,
    /// Parsed identity from binary certificate chain. None if parse failed.
    #[serde(default)]
    pub identity: Option<ProgrammerIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResult {
    pub passed: bool,
    pub bytes_checked: u64,
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_type_display() {
        assert_eq!(ProtocolType::Fastboot.to_string(), "Fastboot");
        assert_eq!(ProtocolType::Adb.to_string(), "ADB");
        assert_eq!(ProtocolType::Edl.to_string(), "EDL");
        assert_eq!(ProtocolType::MtkBrom.to_string(), "MTK BROM");
    }

    #[test]
    fn test_device_info_serializes() {
        let device = DeviceInfo {
            vendor_id: 0x18d1,
            product_id: 0x4EE0,
            serial: Some("RF8N30XXXXX".into()),
            manufacturer: Some("Google".into()),
            product: Some("Pixel 6".into()),
            protocol: ProtocolType::Fastboot,
            adb_state: None,
        };
        let json = serde_json::to_string(&device).unwrap();
        assert!(json.contains("RF8N30XXXXX"));
        assert!(json.contains("Fastboot"));
    }

    #[test]
    fn test_flash_progress_serializes() {
        let progress = FlashProgress {
            stage: FlashStage::Sending,
            message: "Sending firmware...".into(),
            percent: None,
        };
        let json = serde_json::to_string(&progress).unwrap();
        assert!(json.contains("Sending"));

        let with_percent = FlashProgress {
            stage: FlashStage::Sending,
            message: "Sideloading... 42%".into(),
            percent: Some(42.0),
        };
        let json = serde_json::to_string(&with_percent).unwrap();
        assert!(json.contains("42.0"));
    }

    #[test]
    fn test_device_info_deserializes() {
        let json = r#"{
            "vendor_id": 6353,
            "product_id": 20192,
            "serial": "ABC123",
            "manufacturer": "Google",
            "product": "Pixel",
            "protocol": "Fastboot"
        }"#;
        let device: DeviceInfo = serde_json::from_str(json).unwrap();
        assert_eq!(device.vendor_id, 0x18d1);
        assert_eq!(device.protocol, ProtocolType::Fastboot);
    }

    #[test]
    fn test_root_status_serializes() {
        let status = RootStatus {
            root_type: RootType::Su,
            message: "Root via su".into(),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("Su"));
        assert!(json.contains("Root via su"));
    }

    #[test]
    fn test_partition_info_serializes() {
        let part = PartitionInfo {
            name: "boot".into(),
            size_bytes: Some(67108864),
            size_display: "64 MB".into(),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("boot"));
        assert!(json.contains("67108864"));
    }

    #[test]
    fn test_partition_info_unknown_size() {
        let part = PartitionInfo {
            name: "misc".into(),
            size_bytes: None,
            size_display: "?".into(),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("null"));
    }

    #[test]
    fn test_adb_state_serializes() {
        let device = DeviceInfo {
            vendor_id: 0,
            product_id: 0,
            serial: Some("abc123".into()),
            manufacturer: None,
            product: None,
            protocol: ProtocolType::Adb,
            adb_state: Some(AdbState::Recovery),
        };
        let json = serde_json::to_string(&device).unwrap();
        assert!(json.contains("Recovery"));
        assert!(json.contains("adb_state"));
    }

    #[test]
    fn test_adb_state_default_none() {
        let json = r#"{
            "vendor_id": 0,
            "product_id": 0,
            "serial": null,
            "manufacturer": null,
            "product": null,
            "protocol": "Fastboot"
        }"#;
        let device: DeviceInfo = serde_json::from_str(json).unwrap();
        assert_eq!(device.adb_state, None);
    }

    #[test]
    fn test_edl_device_info_serializes() {
        let info = EdlDeviceInfo {
            serial: Some("12345678".into()),
            hw_id: Some("000CC0E100000000".into()),
            pk_hash: Some("AABBCCDD".into()),
            storage_type: Some("ufs".into()),
            sector_size: Some(4096),
            num_luns: Some(6),
            firehose_active: false,
            chipset: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("12345678"));
        assert!(json.contains("ufs"));
        assert!(json.contains("4096"));
    }

    #[test]
    fn test_edl_device_info_partial() {
        let info = EdlDeviceInfo {
            serial: Some("ABCD".into()),
            hw_id: None,
            pk_hash: None,
            storage_type: None,
            sector_size: None,
            num_luns: None,
            firehose_active: false,
            chipset: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("ABCD"));
        assert!(json.contains("null"));
    }

    #[test]
    fn test_edl_partition_entry_serializes() {
        let entry = EdlPartitionEntry {
            name: "boot".into(),
            start_sector: 131072,
            num_sectors: 16384,
            size_bytes: 67108864,
            lun: 0,
            type_guid: "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7".into(),
            unique_guid: "00000000-0000-0000-0000-000000000000".into(),
            attributes: 0,
            category: "boot".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("boot"));
        assert!(json.contains("131072"));
        assert!(json.contains("67108864"));
    }

    #[test]
    fn test_rawprogram_discovery_serializes() {
        let entry = RawprogramDiscovery {
            rawprogram_path: "C:\\fw\\rawprogram0.xml".into(),
            patch_path: Some("C:\\fw\\patch0.xml".into()),
            lun_hint: 0,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("rawprogram0.xml"));
        assert!(json.contains("patch0.xml"));
    }

    #[test]
    fn test_batch_flash_result_serializes() {
        let result = BatchFlashResult {
            programmed: vec!["boot".into(), "system".into()],
            erased: vec!["userdata".into()],
            patched: 3,
            errors: vec![],
            verified: vec![("boot".into(), true), ("system".into(), true)],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("boot"));
        assert!(json.contains("system"));
        assert!(json.contains("userdata"));
        assert!(json.contains(r#""patched":3"#));
        assert!(json.contains(r#"["boot",true]"#));
    }

    #[test]
    fn test_programmer_entry_serializes() {
        let entry = ProgrammerEntry {
            programmer_path: "C:\\fw\\prog.elf".into(),
            programmer_name: "prog.elf".into(),
            device_serial: Some("12345678".into()),
            storage_type: Some("emmc".into()),
            last_used: "2026-04-10T20:00:00Z".into(),
            use_count: 3,
            file_exists: true,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("prog.elf"));
        assert!(json.contains("12345678"));
        assert!(json.contains("use_count"));
        assert!(json.contains("file_exists"));
    }

    #[test]
    fn test_programmer_entry_file_exists_default_false() {
        // Old JSON without file_exists must deserialize with file_exists = false
        let json = r#"{
            "programmer_path": "/fw/prog.elf",
            "programmer_name": "prog.elf",
            "device_serial": null,
            "storage_type": null,
            "last_used": "2026-04-10",
            "use_count": 1
        }"#;
        let entry: ProgrammerEntry = serde_json::from_str(json).unwrap();
        assert!(!entry.file_exists);
    }

    #[test]
    fn test_programmer_candidate_serializes() {
        let c = ProgrammerCandidate {
            name: "prog_firehose_8998.elf".into(),
            path: "/path/to/prog.elf".into(),
            valid: true,
            size_bytes: 409600,
            match_level: MatchLevel::Unknown,
            identity: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("prog_firehose_8998.elf"));
        assert!(json.contains(r#""valid":true"#));
    }

    #[test]
    fn test_verify_result_serializes() {
        let v = VerifyResult {
            passed: true,
            bytes_checked: 2097152,
            detail: "Head and tail match".into(),
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains(r#""passed":true"#));
        assert!(json.contains("2097152"));
    }

    #[test]
    fn test_match_level_ordering() {
        use std::cmp::Ordering;
        assert_eq!(MatchLevel::DbExact.cmp(&MatchLevel::FilenameMatch), Ordering::Less);
        assert_eq!(MatchLevel::FilenameMatch.cmp(&MatchLevel::Unknown), Ordering::Less);
        assert_eq!(MatchLevel::Unknown.cmp(&MatchLevel::DbOtherDevice), Ordering::Less);
        assert_eq!(MatchLevel::default(), MatchLevel::Unknown);
    }

    #[test]
    fn test_match_level_serializes() {
        let level = MatchLevel::DbExact;
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, r#""DbExact""#);

        let level = MatchLevel::Unknown;
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, r#""Unknown""#);
    }

    #[test]
    fn test_programmer_candidate_with_match_level() {
        let c = ProgrammerCandidate {
            name: "prog.elf".into(),
            path: "/path/prog.elf".into(),
            valid: true,
            size_bytes: 1024,
            match_level: MatchLevel::DbExact,
            identity: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""match_level":"DbExact""#));
    }

    #[test]
    fn test_programmer_candidate_default_match_level() {
        // Simulates deserialization from JSON without match_level field (backward compat)
        let json = r#"{"name":"prog.elf","path":"/p","valid":true,"size_bytes":100}"#;
        let c: ProgrammerCandidate = serde_json::from_str(json).unwrap();
        assert_eq!(c.match_level, MatchLevel::Unknown);
    }
}
