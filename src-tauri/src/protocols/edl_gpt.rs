//! GPT (GUID Partition Table) parser for EDL partition listing.
//!
//! Parses UEFI GPT headers and partition entries from raw sector data
//! read via Firehose. Pure parsing — no protocol I/O.
//!
//! Reference: UEFI Specification, Chapter 5 — GUID Partition Table Format

use crate::error::FlashError;
use crate::types::EdlPartitionEntry;

/// Format a 16-byte GUID as standard string (mixed-endian per UEFI spec).
fn format_guid(bytes: &[u8]) -> String {
    if bytes.len() < 16 {
        return "00000000-0000-0000-0000-000000000000".to_string();
    }
    let a = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let b = u16::from_le_bytes([bytes[4], bytes[5]]);
    let c = u16::from_le_bytes([bytes[6], bytes[7]]);
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        a, b, c,
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// Map partition name to category for color coding.
fn partition_category(name: &str) -> &'static str {
    let base = name.strip_suffix("_a")
        .or_else(|| name.strip_suffix("_b"))
        .unwrap_or(name);

    match base {
        "boot" | "recovery" | "dtbo" | "vbmeta" | "init_boot"
        | "vbmeta_system" | "vbmeta_vendor" => "boot",

        "system" | "vendor" | "odm" | "product" | "system_ext"
        | "vendor_dlkm" | "odm_dlkm" | "system_dlkm" => "system",

        "xbl" | "abl" | "tz" | "hyp" | "rpm" | "devcfg" | "modem"
        | "bluetooth" | "dsp" | "aop" | "qupfw" | "shrm" | "uefi"
        | "xbl_config" | "multiimgoem" | "imagefv" | "featenabler" => "firmware",

        "userdata" => "userdata",

        "misc" | "metadata" | "persist" | "frp" | "fsc" | "fsg"
        | "devinfo" | "config" | "logfs" | "limits" | "spunvm"
        | "last_parti" | "cdt" | "ddr" => "metadata",

        _ => "unknown",
    }
}

/// Parse GPT header (at LBA 1). Returns (num_entries, entry_size, entry_start_lba).
fn parse_gpt_header(header: &[u8]) -> Result<(u32, u32, u64), FlashError> {
    if header.len() < 92 {
        return Err(FlashError::Protocol("GPT header too short".into()));
    }
    if &header[0..8] != b"EFI PART" {
        return Err(FlashError::Protocol("Invalid GPT signature".into()));
    }

    let entry_lba = u64::from_le_bytes(header[72..80].try_into().expect("len >= 92"));
    let num_entries = u32::from_le_bytes(header[80..84].try_into().expect("len >= 92"));
    let entry_size = u32::from_le_bytes(header[84..88].try_into().expect("len >= 92"));

    if entry_size < 128 {
        return Err(FlashError::Protocol(format!(
            "Invalid GPT entry size: {entry_size}"
        )));
    }

    Ok((num_entries, entry_size, entry_lba))
}

/// Parse a single GPT partition entry. Returns None if entry is empty.
fn parse_gpt_entry(entry: &[u8], sector_size: u32, lun: u8) -> Option<EdlPartitionEntry> {
    if entry.len() < 128 {
        return None;
    }

    if entry[0..16].iter().all(|&b| b == 0) {
        return None;
    }

    let type_guid = format_guid(&entry[0..16]);
    let unique_guid = format_guid(&entry[16..32]);
    let first_lba = u64::from_le_bytes(entry[32..40].try_into().expect("len >= 128"));
    let last_lba = u64::from_le_bytes(entry[40..48].try_into().expect("len >= 128"));
    if last_lba < first_lba {
        return None; // Corrupted entry
    }
    let attributes = u64::from_le_bytes(entry[48..56].try_into().expect("len >= 128"));
    let num_sectors = last_lba - first_lba + 1;

    let name_bytes = &entry[56..128];
    let name_u16: Vec<u16> = name_bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    let name = String::from_utf16_lossy(&name_u16);
    let category = partition_category(&name).to_string();

    Some(EdlPartitionEntry {
        name,
        start_sector: first_lba,
        num_sectors,
        size_bytes: num_sectors * sector_size as u64,
        lun,
        type_guid,
        unique_guid,
        attributes,
        category,
    })
}

/// Parse all partitions from raw GPT data (LBA 0-33).
pub fn parse_gpt(
    raw_data: &[u8],
    sector_size: u32,
    lun: u8,
) -> Result<Vec<EdlPartitionEntry>, FlashError> {
    if raw_data.len() < (sector_size as usize) * 2 {
        return Err(FlashError::Protocol("GPT data too short".into()));
    }

    let header_offset = sector_size as usize; // LBA 1
    let header = &raw_data[header_offset..header_offset + sector_size as usize];
    let (num_entries, entry_size, entry_lba) = parse_gpt_header(header)?;

    let entries_offset = (entry_lba as usize) * (sector_size as usize);
    let mut partitions = Vec::new();

    for i in 0..num_entries {
        let offset = entries_offset + (i as usize) * (entry_size as usize);
        if offset + entry_size as usize > raw_data.len() {
            break;
        }
        let entry_data = &raw_data[offset..offset + entry_size as usize];
        if let Some(part) = parse_gpt_entry(entry_data, sector_size, lun) {
            partitions.push(part);
        }
    }

    Ok(partitions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gpt_header() {
        let mut header = vec![0u8; 512];
        header[0..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x00010000u32.to_le_bytes());
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        header[72..80].copy_from_slice(&2u64.to_le_bytes());
        header[80..84].copy_from_slice(&2u32.to_le_bytes());
        header[84..88].copy_from_slice(&128u32.to_le_bytes());

        let (num_entries, entry_size, entry_lba) = parse_gpt_header(&header).unwrap();
        assert_eq!(num_entries, 2);
        assert_eq!(entry_size, 128);
        assert_eq!(entry_lba, 2);
    }

    #[test]
    fn test_parse_gpt_entry() {
        let mut entry = vec![0u8; 128];
        entry[0] = 0x28;
        entry[1] = 0x73;
        entry[32..40].copy_from_slice(&1024u64.to_le_bytes());
        entry[40..48].copy_from_slice(&2047u64.to_le_bytes());
        let name_bytes: Vec<u8> = "boot"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        entry[56..56 + name_bytes.len()].copy_from_slice(&name_bytes);

        let result = parse_gpt_entry(&entry, 512, 0);
        assert!(result.is_some());
        let part = result.unwrap();
        assert_eq!(part.name, "boot");
        assert_eq!(part.start_sector, 1024);
        assert_eq!(part.num_sectors, 1024);
        assert_eq!(part.size_bytes, 1024 * 512);
        assert_eq!(part.lun, 0);
    }

    #[test]
    fn test_parse_gpt_entry_empty() {
        let entry = vec![0u8; 128];
        assert!(parse_gpt_entry(&entry, 512, 0).is_none());
    }

    #[test]
    fn test_parse_gpt_invalid_signature() {
        let header = vec![0u8; 512];
        assert!(parse_gpt_header(&header).is_err());
    }

    #[test]
    fn test_parse_gpt_full() {
        let sector_size: u32 = 512;
        let num_lba = 34;
        let mut data = vec![0u8; num_lba * sector_size as usize];

        // LBA 1: GPT header
        let hdr_off = sector_size as usize;
        data[hdr_off..hdr_off + 8].copy_from_slice(b"EFI PART");
        data[hdr_off + 72..hdr_off + 80].copy_from_slice(&2u64.to_le_bytes()); // entry LBA
        data[hdr_off + 80..hdr_off + 84].copy_from_slice(&128u32.to_le_bytes()); // num entries
        data[hdr_off + 84..hdr_off + 88].copy_from_slice(&128u32.to_le_bytes()); // entry size

        // LBA 2: first partition entry
        let ent_off = 2 * sector_size as usize;
        data[ent_off] = 0x28; // non-zero type GUID
        data[ent_off + 32..ent_off + 40].copy_from_slice(&100u64.to_le_bytes());
        data[ent_off + 40..ent_off + 48].copy_from_slice(&199u64.to_le_bytes());
        let name: Vec<u8> = "system"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        data[ent_off + 56..ent_off + 56 + name.len()].copy_from_slice(&name);

        let parts = parse_gpt(&data, sector_size, 0).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "system");
        assert_eq!(parts[0].num_sectors, 100);
    }

    #[test]
    fn test_format_guid() {
        let bytes: [u8; 16] = [
            0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44,
            0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
        ];
        assert_eq!(
            format_guid(&bytes),
            "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
        );
    }

    #[test]
    fn test_format_guid_zeros() {
        let bytes = [0u8; 16];
        assert_eq!(format_guid(&bytes), "00000000-0000-0000-0000-000000000000");
    }

    #[test]
    fn test_partition_category() {
        assert_eq!(partition_category("boot"), "boot");
        assert_eq!(partition_category("boot_a"), "boot");
        assert_eq!(partition_category("recovery"), "boot");
        assert_eq!(partition_category("system"), "system");
        assert_eq!(partition_category("system_ext"), "system");
        assert_eq!(partition_category("vendor"), "system");
        assert_eq!(partition_category("modem"), "firmware");
        assert_eq!(partition_category("xbl"), "firmware");
        assert_eq!(partition_category("tz"), "firmware");
        assert_eq!(partition_category("userdata"), "userdata");
        assert_eq!(partition_category("misc"), "metadata");
        assert_eq!(partition_category("persist"), "metadata");
        assert_eq!(partition_category("splash"), "unknown");
        assert_eq!(partition_category("custom_part"), "unknown");
    }

    #[test]
    fn test_parse_gpt_entry_extracts_guid_and_attributes() {
        let mut entry = [0u8; 128];
        entry[0..16].copy_from_slice(&[
            0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44,
            0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
        ]);
        entry[16..32].copy_from_slice(&[0x11; 16]);
        entry[32..40].copy_from_slice(&2048u64.to_le_bytes());
        entry[40..48].copy_from_slice(&4095u64.to_le_bytes());
        entry[48..56].copy_from_slice(&1u64.to_le_bytes());
        let name_utf16: Vec<u8> = "boot".encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        entry[56..56 + name_utf16.len()].copy_from_slice(&name_utf16);

        let part = parse_gpt_entry(&entry, 512, 0).unwrap();
        assert_eq!(part.type_guid, "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7");
        assert_eq!(part.attributes, 1);
        assert_eq!(part.category, "boot");
        assert_eq!(part.start_sector, 2048);
        assert_eq!(part.num_sectors, 2048);
    }
}
