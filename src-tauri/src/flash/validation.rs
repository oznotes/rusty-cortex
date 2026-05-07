use std::path::Path;

use crate::error::FlashError;

/// Maximum firmware file size: 8 GB
const MAX_FIRMWARE_SIZE: u64 = 8 * 1024 * 1024 * 1024;

/// Allowed firmware file extensions
const ALLOWED_EXTENSIONS: &[&str] = &["img", "bin", "mbn", "elf", "raw"];

/// Validate a firmware file before flashing.
pub fn validate_firmware(path: &Path) -> Result<(), FlashError> {
    if !path.exists() {
        return Err(FlashError::Validation(format!(
            "File not found: {}", path.display()
        )));
    }
    if !path.is_file() {
        return Err(FlashError::Validation(format!(
            "Not a file: {}", path.display()
        )));
    }
    let ext = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if !ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
        return Err(FlashError::Validation(format!(
            "Unsupported file type '.{}'. Expected one of: {}",
            ext, ALLOWED_EXTENSIONS.join(", ")
        )));
    }

    let metadata = std::fs::metadata(path)?;
    if metadata.len() == 0 {
        return Err(FlashError::Validation("Firmware file is empty".into()));
    }
    if metadata.len() > MAX_FIRMWARE_SIZE {
        return Err(FlashError::Validation(format!(
            "Firmware file too large: {} bytes (max {} bytes)",
            metadata.len(), MAX_FIRMWARE_SIZE
        )));
    }

    Ok(())
}

/// Validate a partition name.
pub fn validate_partition(partition: &str) -> Result<(), FlashError> {
    if partition.is_empty() {
        return Err(FlashError::Validation("Partition name is empty".into()));
    }
    if !partition.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        return Err(FlashError::Validation(format!(
            "Invalid partition name '{}': only alphanumeric, underscore, and hyphen allowed",
            partition
        )));
    }
    Ok(())
}

/// Critical partitions that require extra confirmation.
const CRITICAL_PARTITIONS: &[&str] = &[
    "bootloader", "modem", "aboot", "sbl1", "tz", "rpm", "hyp", "xbl", "aop",
];

/// Check if a partition is considered critical.
pub fn is_critical_partition(partition: &str) -> bool {
    CRITICAL_PARTITIONS.iter().any(|&p| p.eq_ignore_ascii_case(partition))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn create_temp_firmware(ext: &str, size: usize) -> tempfile::NamedTempFile {
        let suffix = format!(".{}", ext);
        let mut f = tempfile::Builder::new()
            .suffix(&suffix)
            .tempfile()
            .unwrap();
        f.write_all(&vec![0u8; size]).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn test_validate_firmware_valid_img() {
        let f = create_temp_firmware("img", 1024);
        assert!(validate_firmware(f.path()).is_ok());
    }

    #[test]
    fn test_validate_firmware_valid_bin() {
        let f = create_temp_firmware("bin", 1024);
        assert!(validate_firmware(f.path()).is_ok());
    }

    #[test]
    fn test_validate_firmware_not_found() {
        let result = validate_firmware(Path::new("/nonexistent/file.img"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_validate_firmware_empty() {
        let f = create_temp_firmware("img", 0);
        let result = validate_firmware(f.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn test_validate_firmware_bad_extension() {
        let f = create_temp_firmware("txt", 1024);
        let result = validate_firmware(f.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unsupported"));
    }

    #[test]
    fn test_validate_partition_valid() {
        assert!(validate_partition("boot").is_ok());
        assert!(validate_partition("system_a").is_ok());
        assert!(validate_partition("vendor-b").is_ok());
    }

    #[test]
    fn test_validate_partition_empty() {
        let result = validate_partition("");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_partition_invalid_chars() {
        let result = validate_partition("boot;rm -rf /");
        assert!(result.is_err());
    }

    #[test]
    fn test_critical_partition_detection() {
        assert!(is_critical_partition("bootloader"));
        assert!(is_critical_partition("BOOTLOADER"));
        assert!(is_critical_partition("modem"));
        assert!(!is_critical_partition("boot"));
        assert!(!is_critical_partition("recovery"));
        assert!(!is_critical_partition("system"));
    }
}
