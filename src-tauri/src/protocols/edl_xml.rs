use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;
use tracing::warn;

use crate::error::FlashError;

// ---------------------------------------------------------------------------
// Data structs (internal only — no Serialize/Deserialize)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RawProgramEntry {
    pub filename: String,
    pub label: String,
    pub start_sector: u64,
    pub num_partition_sectors: u64,
    pub physical_partition_number: u8,
    pub _file_sector_offset: u64,
    pub _sector_size: u32,
}

#[derive(Debug, Clone)]
pub struct RawEraseEntry {
    pub start_sector: u64,
    pub num_partition_sectors: u64,
    pub physical_partition_number: u8,
    pub _sector_size: u32,
}

#[derive(Debug, Clone)]
pub struct PatchEntry {
    pub byte_offset: u64,
    pub physical_partition_number: u8,
    pub size_in_bytes: u64,
    /// String, not u64 — patch.xml values can contain expressions like "NUM_DISK_SECTORS-33."
    /// which the device-side Firehose programmer evaluates.
    pub start_sector: String,
    /// String — may contain hex values or expressions evaluated by the device.
    pub value: String,
    pub _sector_size: u32,
}

/// A discovered rawprogram.xml + optional matching patch.xml pair.
#[derive(Debug, Clone)]
pub struct RawprogramSet {
    pub rawprogram_path: PathBuf,
    pub patch_path: Option<PathBuf>,
    /// LUN hint from filename (rawprogram2.xml -> 2). 0 if unnumbered.
    pub lun_hint: u8,
}

// ---------------------------------------------------------------------------
// Private helpers — attribute extraction
// ---------------------------------------------------------------------------

fn attr_str(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().filter_map(|a| a.ok()).find_map(|a| {
        if a.key.as_ref() == key {
            Some(String::from_utf8_lossy(&a.value).into_owned())
        } else {
            None
        }
    })
}

fn attr_num<T: std::str::FromStr + Default>(e: &quick_xml::events::BytesStart, key: &[u8]) -> T {
    let key_str = std::str::from_utf8(key).unwrap_or("?");
    match attr_str(e, key) {
        Some(s) => s.parse::<T>().unwrap_or_else(|_| {
            warn!("XML attribute '{}' has non-numeric value '{}', defaulting to 0", key_str, s);
            T::default()
        }),
        None => T::default(),
    }
}

// ---------------------------------------------------------------------------
// Private helpers — parse element attributes into structs
// ---------------------------------------------------------------------------

fn parse_program_attrs(e: &quick_xml::events::BytesStart) -> Option<RawProgramEntry> {
    let filename = attr_str(e, b"filename").unwrap_or_default();
    let num_partition_sectors = attr_num::<u64>(e, b"num_partition_sectors");

    // Skip placeholders: empty filename or zero sectors.
    if filename.is_empty() || num_partition_sectors == 0 {
        return None;
    }

    Some(RawProgramEntry {
        filename,
        label: attr_str(e, b"label").unwrap_or_default(),
        start_sector: attr_num::<u64>(e, b"start_sector"),
        num_partition_sectors,
        physical_partition_number: attr_num::<u8>(e, b"physical_partition_number"),
        _file_sector_offset: attr_num::<u64>(e, b"file_sector_offset"),
        _sector_size: attr_num::<u32>(e, b"SECTOR_SIZE_IN_BYTES"),
    })
}

fn parse_erase_attrs(e: &quick_xml::events::BytesStart) -> RawEraseEntry {
    RawEraseEntry {
        start_sector: attr_num::<u64>(e, b"start_sector"),
        num_partition_sectors: attr_num::<u64>(e, b"num_partition_sectors"),
        physical_partition_number: attr_num::<u8>(e, b"physical_partition_number"),
        _sector_size: attr_num::<u32>(e, b"SECTOR_SIZE_IN_BYTES"),
    }
}

fn parse_patch_attrs(e: &quick_xml::events::BytesStart) -> PatchEntry {
    PatchEntry {
        byte_offset: attr_num::<u64>(e, b"byte_offset"),
        physical_partition_number: attr_num::<u8>(e, b"physical_partition_number"),
        size_in_bytes: attr_num::<u64>(e, b"size_in_bytes"),
        start_sector: attr_str(e, b"start_sector").unwrap_or_default(),
        value: attr_str(e, b"value").unwrap_or_default(),
        _sector_size: attr_num::<u32>(e, b"SECTOR_SIZE_IN_BYTES"),
    }
}

// ---------------------------------------------------------------------------
// Public parsers
// ---------------------------------------------------------------------------

/// Parse a rawprogram XML manifest, returning program entries and erase entries.
///
/// Iterates self-closing `<program .../>` and `<erase .../>` elements.
/// Placeholder program entries (empty filename or zero `num_partition_sectors`)
/// are skipped.
pub fn parse_rawprogram(
    path: &Path,
) -> Result<(Vec<RawProgramEntry>, Vec<RawEraseEntry>), FlashError> {
    let xml = std::fs::read_to_string(path)?;
    let mut reader = Reader::from_str(&xml);

    let mut programs: Vec<RawProgramEntry> = Vec::new();
    let mut erases: Vec<RawEraseEntry> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Empty(ref e)) => {
                let tag = e.name();
                if tag.as_ref() == b"program" {
                    if let Some(entry) = parse_program_attrs(e) {
                        programs.push(entry);
                    }
                } else if tag.as_ref() == b"erase" {
                    erases.push(parse_erase_attrs(e));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(FlashError::Protocol(format!(
                    "Failed to parse rawprogram XML: {e}"
                )));
            }
            _ => {}
        }
    }

    Ok((programs, erases))
}

/// Parse a patch XML manifest, returning only entries targeting `DISK`.
///
/// Entries whose `filename` attribute is not `"DISK"` (e.g. targeting a
/// specific file like `gpt_main0.bin`) are filtered out — EDL applies those
/// differently.
pub fn parse_patch_xml(path: &Path) -> Result<Vec<PatchEntry>, FlashError> {
    let xml = std::fs::read_to_string(path)?;
    let mut reader = Reader::from_str(&xml);

    let mut patches: Vec<PatchEntry> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Empty(ref e)) => {
                if e.name().as_ref() == b"patch" {
                    let filename = attr_str(e, b"filename").unwrap_or_default();
                    if filename == "DISK" {
                        patches.push(parse_patch_attrs(e));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(FlashError::Protocol(format!(
                    "Failed to parse patch XML: {e}"
                )));
            }
            _ => {}
        }
    }

    Ok(patches)
}

// ---------------------------------------------------------------------------
// Directory discovery — multi-rawprogram support
// ---------------------------------------------------------------------------

/// Discover rawprogram*.xml files in a directory.
/// Returns sorted by LUN. Filters out WIPE/BLANK variants.
pub fn discover_rawprograms(dir: &Path) -> Result<Vec<RawprogramSet>, FlashError> {
    if !dir.is_dir() {
        return Err(FlashError::Validation(format!(
            "Not a directory: {}",
            dir.display()
        )));
    }

    let entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .collect();

    let mut sets: Vec<RawprogramSet> = Vec::new();

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if !name.starts_with("rawprogram") || !name.ends_with(".xml") {
            continue;
        }
        // Filter dangerous variants
        if name.contains("wipe") || name.contains("blank") {
            continue;
        }

        let lun_hint = extract_lun_hint(&name);
        let patch_path = find_matching_patch(dir, &entries, lun_hint);

        sets.push(RawprogramSet {
            rawprogram_path: entry.path(),
            patch_path,
            lun_hint,
        });
    }

    sets.sort_by_key(|s| s.lun_hint);
    sets.dedup_by_key(|s| s.lun_hint);

    Ok(sets)
}

/// Extract LUN number from filename. rawprogram2.xml -> 2, rawprogram.xml -> 0.
fn extract_lun_hint(filename: &str) -> u8 {
    let stem = filename.strip_suffix(".xml").unwrap_or(filename);
    let after_prefix = stem.strip_prefix("rawprogram").unwrap_or("");

    // First char might be a digit (rawprogram0, rawprogram5)
    if let Some(ch) = after_prefix.chars().next() {
        if let Some(d) = ch.to_digit(10) {
            return d as u8;
        }
    }

    // Could be rawprogram_unsparse0.xml — look for trailing digit
    if let Some(ch) = after_prefix.chars().last() {
        if let Some(d) = ch.to_digit(10) {
            return d as u8;
        }
    }

    0
}

/// Find a matching patchN.xml for a given LUN hint.
fn find_matching_patch(
    dir: &Path,
    entries: &[std::fs::DirEntry],
    lun_hint: u8,
) -> Option<PathBuf> {
    let patch_name = format!("patch{lun_hint}.xml");
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name == patch_name {
            return Some(entry.path());
        }
    }
    if lun_hint == 0 {
        let patch_generic = dir.join("patch.xml");
        if patch_generic.exists() {
            return Some(patch_generic);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_parse_rawprogram_basic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rawprogram0.xml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"<?xml version="1.0" ?>
<data>
  <program filename="boot.img" label="boot" start_sector="131072" num_partition_sectors="65536" physical_partition_number="0" file_sector_offset="0" SECTOR_SIZE_IN_BYTES="4096" />
  <program filename="system.img" label="system" start_sector="262144" num_partition_sectors="524288" physical_partition_number="0" file_sector_offset="0" SECTOR_SIZE_IN_BYTES="4096" />
</data>"#
        )
        .unwrap();

        let (programs, erases) = parse_rawprogram(&path).unwrap();

        assert_eq!(programs.len(), 2);
        assert!(erases.is_empty());

        assert_eq!(programs[0].filename, "boot.img");
        assert_eq!(programs[0].label, "boot");
        assert_eq!(programs[0].start_sector, 131072);
        assert_eq!(programs[0].num_partition_sectors, 65536);
        assert_eq!(programs[0].physical_partition_number, 0);
        assert_eq!(programs[0]._file_sector_offset, 0);
        assert_eq!(programs[0]._sector_size, 4096);

        assert_eq!(programs[1].filename, "system.img");
        assert_eq!(programs[1].label, "system");
        assert_eq!(programs[1].start_sector, 262144);
        assert_eq!(programs[1].num_partition_sectors, 524288);
    }

    #[test]
    fn test_parse_rawprogram_with_erase() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rawprogram0.xml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"<?xml version="1.0" ?>
<data>
  <program filename="boot.img" label="boot" start_sector="131072" num_partition_sectors="65536" physical_partition_number="0" file_sector_offset="0" SECTOR_SIZE_IN_BYTES="4096" />
  <erase start_sector="0" num_partition_sectors="32768" physical_partition_number="0" SECTOR_SIZE_IN_BYTES="4096" />
</data>"#
        )
        .unwrap();

        let (programs, erases) = parse_rawprogram(&path).unwrap();

        assert_eq!(programs.len(), 1);
        assert_eq!(erases.len(), 1);

        assert_eq!(erases[0].start_sector, 0);
        assert_eq!(erases[0].num_partition_sectors, 32768);
        assert_eq!(erases[0].physical_partition_number, 0);
        assert_eq!(erases[0]._sector_size, 4096);
    }

    #[test]
    fn test_parse_rawprogram_skips_empty_filename() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rawprogram0.xml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"<?xml version="1.0" ?>
<data>
  <program filename="" label="placeholder" start_sector="0" num_partition_sectors="100" physical_partition_number="0" file_sector_offset="0" SECTOR_SIZE_IN_BYTES="4096" />
  <program filename="boot.img" label="boot" start_sector="131072" num_partition_sectors="65536" physical_partition_number="0" file_sector_offset="0" SECTOR_SIZE_IN_BYTES="4096" />
</data>"#
        )
        .unwrap();

        let (programs, _) = parse_rawprogram(&path).unwrap();

        assert_eq!(programs.len(), 1);
        assert_eq!(programs[0].filename, "boot.img");
    }

    #[test]
    fn test_parse_rawprogram_skips_zero_sectors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rawprogram0.xml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"<?xml version="1.0" ?>
<data>
  <program filename="zeros.img" label="zeros" start_sector="0" num_partition_sectors="0" physical_partition_number="0" file_sector_offset="0" SECTOR_SIZE_IN_BYTES="4096" />
  <program filename="boot.img" label="boot" start_sector="131072" num_partition_sectors="65536" physical_partition_number="0" file_sector_offset="0" SECTOR_SIZE_IN_BYTES="4096" />
</data>"#
        )
        .unwrap();

        let (programs, _) = parse_rawprogram(&path).unwrap();

        assert_eq!(programs.len(), 1);
        assert_eq!(programs[0].filename, "boot.img");
    }

    #[test]
    fn test_parse_patch_xml() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("patch0.xml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"<?xml version="1.0" ?>
<patches>
  <patch filename="DISK" byte_offset="544" physical_partition_number="0" size_in_bytes="8" start_sector="NUM_DISK_SECTORS-33." value="NUM_DISK_SECTORS-33." SECTOR_SIZE_IN_BYTES="4096" />
  <patch filename="DISK" byte_offset="1024" physical_partition_number="0" size_in_bytes="4" start_sector="0" value="100" SECTOR_SIZE_IN_BYTES="4096" />
</patches>"#
        )
        .unwrap();

        let patches = parse_patch_xml(&path).unwrap();

        assert_eq!(patches.len(), 2);

        assert_eq!(patches[0].byte_offset, 544);
        assert_eq!(patches[0].physical_partition_number, 0);
        assert_eq!(patches[0].size_in_bytes, 8);
        assert_eq!(patches[0].start_sector, "NUM_DISK_SECTORS-33.");
        assert_eq!(patches[0].value, "NUM_DISK_SECTORS-33.");
        assert_eq!(patches[0]._sector_size, 4096);

        assert_eq!(patches[1].byte_offset, 1024);
        assert_eq!(patches[1].size_in_bytes, 4);
        assert_eq!(patches[1].value, "100");
    }

    #[test]
    fn test_parse_patch_filters_disk_only() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("patch0.xml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"<?xml version="1.0" ?>
<patches>
  <patch filename="DISK" byte_offset="544" physical_partition_number="0" size_in_bytes="8" start_sector="0" value="100" SECTOR_SIZE_IN_BYTES="4096" />
  <patch filename="gpt_main0.bin" byte_offset="0" physical_partition_number="0" size_in_bytes="4" start_sector="0" value="200" SECTOR_SIZE_IN_BYTES="4096" />
</patches>"#
        )
        .unwrap();

        let patches = parse_patch_xml(&path).unwrap();

        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].byte_offset, 544);
    }

    // --- discover_rawprograms tests ---

    #[test]
    fn test_discover_rawprograms_numbered() {
        let dir = tempdir().unwrap();
        for i in 0..3 {
            std::fs::write(
                dir.path().join(format!("rawprogram{i}.xml")),
                "<data></data>",
            )
            .unwrap();
            std::fs::write(
                dir.path().join(format!("patch{i}.xml")),
                "<patches></patches>",
            )
            .unwrap();
        }

        let sets = discover_rawprograms(dir.path()).unwrap();
        assert_eq!(sets.len(), 3);
        assert_eq!(sets[0].lun_hint, 0);
        assert_eq!(sets[1].lun_hint, 1);
        assert_eq!(sets[2].lun_hint, 2);
        assert!(sets[0].patch_path.is_some());
        assert!(sets[1].patch_path.is_some());
        assert!(sets[2].patch_path.is_some());
    }

    #[test]
    fn test_discover_rawprograms_unnumbered() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("rawprogram.xml"), "<data></data>").unwrap();

        let sets = discover_rawprograms(dir.path()).unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].lun_hint, 0);
        assert!(sets[0].patch_path.is_none());
    }

    #[test]
    fn test_discover_rawprograms_filters_wipe() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("rawprogram0.xml"), "<data></data>").unwrap();
        std::fs::write(
            dir.path().join("rawprogram0_WIPE_PARTITIONS.xml"),
            "<data></data>",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("rawprogram0_BLANK_GPT.xml"),
            "<data></data>",
        )
        .unwrap();

        let sets = discover_rawprograms(dir.path()).unwrap();
        assert_eq!(sets.len(), 1);
    }

    #[test]
    fn test_discover_rawprograms_empty_dir() {
        let dir = tempdir().unwrap();
        let sets = discover_rawprograms(dir.path()).unwrap();
        assert!(sets.is_empty());
    }

    #[test]
    fn test_discover_rawprograms_unsparse() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("rawprogram_unsparse.xml"),
            "<data></data>",
        )
        .unwrap();
        std::fs::write(dir.path().join("patch0.xml"), "<patches></patches>").unwrap();

        let sets = discover_rawprograms(dir.path()).unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].lun_hint, 0);
        // patch0.xml should match LUN 0
        assert!(sets[0].patch_path.is_some());
    }
}
