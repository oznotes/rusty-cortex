use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::error::FlashError;
use crate::protocols::edl_mbn::{is_valid_programmer_magic, parse_programmer_identity};
use crate::types::{MatchLevel, ProgrammerCandidate, ProgrammerEntry, ProgrammerIdentity};

// ---------------------------------------------------------------------------
// On-disk format
// ---------------------------------------------------------------------------

const DB_FILENAME: &str = "programmer_database.json";
const DB_VERSION: u32 = 1;

/// Number of hex characters from PKHash used for matching (bkerler convention).
/// 16 hex chars = 8 bytes of the SHA256/384 hash.
pub const PKHASH_PREFIX_LEN: usize = 16;

#[derive(Serialize, Deserialize)]
struct DatabaseFile {
    version: u32,
    entries: HashMap<String, ProgrammerEntry>,
}

#[derive(Serialize)]
struct DatabaseFileRef<'a> {
    version: u32,
    entries: &'a HashMap<String, ProgrammerEntry>,
}

// ---------------------------------------------------------------------------
// ProgrammerDatabase
// ---------------------------------------------------------------------------

pub struct ProgrammerDatabase {
    pub entries: HashMap<String, ProgrammerEntry>,
    db_path: PathBuf,
}

impl ProgrammerDatabase {
    /// Build the canonical key for a device: lowercase `{hwid}:{pkhash}`.
    pub fn make_key(hwid: &str, pkhash: &str) -> String {
        format!("{}:{}", hwid.to_lowercase(), pkhash.to_lowercase())
    }

    /// Load the database from `{app_data_dir}/programmer_database.json`.
    /// Returns an empty database if the file is missing or corrupt.
    pub fn load(app_data_dir: &Path) -> Self {
        let db_path = app_data_dir.join(DB_FILENAME);

        let data = match std::fs::read_to_string(&db_path) {
            Ok(d) => d,
            Err(_) => {
                info!("Programmer database not found at {}, starting fresh", db_path.display());
                return Self {
                    entries: HashMap::new(),
                    db_path,
                };
            }
        };

        match serde_json::from_str::<DatabaseFile>(&data) {
            Ok(db_file) => {
                info!(
                    "Loaded programmer database ({} entries, version {})",
                    db_file.entries.len(),
                    db_file.version,
                );
                Self {
                    entries: db_file.entries,
                    db_path,
                }
            }
            Err(e) => {
                warn!("Programmer database corrupt ({}), starting fresh", e);
                Self {
                    entries: HashMap::new(),
                    db_path,
                }
            }
        }
    }

    /// Save the database to disk.
    pub fn save(&self) -> Result<(), FlashError> {
        let db_file = DatabaseFileRef {
            version: DB_VERSION,
            entries: &self.entries,
        };

        let json = serde_json::to_string_pretty(&db_file)
            .map_err(|e| FlashError::Protocol(format!("Failed to serialize programmer database: {e}")))?;

        // Ensure parent directory exists.
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(&self.db_path, json)?;
        info!("Saved programmer database ({} entries)", self.entries.len());
        Ok(())
    }

    /// Create an empty database with no path (for initial AppState).
    pub fn empty() -> Self {
        Self {
            entries: HashMap::new(),
            db_path: PathBuf::new(),
        }
    }

    /// Returns `true` if the database was loaded from (or bound to) a file path.
    pub fn is_loaded(&self) -> bool {
        self.db_path != PathBuf::new()
    }

    /// Lookup a programmer by its raw key (`{hwid}:{pkhash}`), enriched with a
    /// live `file_exists` check.  The caller receives the full entry regardless
    /// of whether the file is present — `file_exists` tells the frontend which
    /// case it is, so it can surface a proper warning instead of a silent miss.
    pub fn lookup_by_key(&self, key: &str) -> Option<ProgrammerEntry> {
        self.entries.get(key).map(|entry| {
            let mut result = entry.clone();
            result.file_exists = std::path::Path::new(&result.programmer_path).exists();
            result
        })
    }

    /// Add or update a programmer entry.
    /// If the key already exists, increments `use_count` and updates the entry's
    /// metadata (path, name, serial, storage_type, last_used).
    pub fn add(&mut self, hwid: &str, pkhash: &str, entry: ProgrammerEntry) {
        let key = Self::make_key(hwid, pkhash);
        if let Some(existing) = self.entries.get_mut(&key) {
            existing.use_count += 1;
            existing.programmer_path = entry.programmer_path;
            existing.programmer_name = entry.programmer_name;
            existing.device_serial = entry.device_serial;
            existing.storage_type = entry.storage_type;
            existing.last_used = entry.last_used;
        } else {
            self.entries.insert(key, entry);
        }
    }

    /// Remove an entry by its key. Returns `true` if the key existed.
    pub fn remove(&mut self, key: &str) -> bool {
        self.entries.remove(key).is_some()
    }

    /// List all entries as `(key, entry)` pairs, sorted by key.
    pub fn list(&self) -> Vec<(String, ProgrammerEntry)> {
        let mut pairs: Vec<(String, ProgrammerEntry)> = self
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
    }
}

// ---------------------------------------------------------------------------
// Standalone scanner
// ---------------------------------------------------------------------------

/// Scan a directory for .elf, .mbn, and .bin programmer files (max depth 2).
///
/// Each file is checked against known magic bytes to set the `valid` flag.
/// If `identity_cache` is provided, parsed [`ProgrammerIdentity`] values are
/// looked up (and stored) by `(path, file_size)` key so repeated scans of the
/// same folder avoid re-parsing.
/// Results are sorted by name (case-insensitive).
pub fn scan_programmers(
    dir: &Path,
    mut identity_cache: Option<&mut HashMap<(String, u64), ProgrammerIdentity>>,
) -> Result<Vec<ProgrammerCandidate>, FlashError> {
    if !dir.is_dir() {
        return Err(FlashError::Validation(format!(
            "Not a directory: {}",
            dir.display()
        )));
    }

    let mut candidates: Vec<ProgrammerCandidate> = Vec::new();
    collect_programmer_files(dir, 0, 2, &mut candidates)?;

    // Parse identity for each candidate, using the cache when available.
    for candidate in candidates.iter_mut() {
        let cache_key = (candidate.path.clone(), candidate.size_bytes);

        candidate.identity = if let Some(ref mut cache) = identity_cache {
            if let Some(cached) = cache.get(&cache_key) {
                Some(cached.clone())
            } else {
                let parsed = parse_programmer_identity(Path::new(&candidate.path));
                if let Some(ref id) = parsed {
                    cache.insert(cache_key, id.clone());
                }
                parsed
            }
        } else {
            parse_programmer_identity(Path::new(&candidate.path))
        };
    }

    candidates.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(candidates)
}

/// Recursively collect .elf/.mbn/.bin files up to `max_depth`.
fn collect_programmer_files(
    dir: &Path,
    current_depth: u32,
    max_depth: u32,
    out: &mut Vec<ProgrammerCandidate>,
) -> Result<(), FlashError> {
    if current_depth > max_depth {
        return Ok(());
    }

    let entries = std::fs::read_dir(dir)?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            collect_programmer_files(&path, current_depth + 1, max_depth, out)?;
        } else if path.is_file() {
            let name = match path.file_name() {
                Some(n) => n.to_string_lossy().into_owned(),
                None => continue,
            };

            let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
            if ext != "elf" && ext != "mbn" && ext != "bin" {
                continue;
            }

            let meta = std::fs::metadata(&path)?;
            let valid = {
                let mut file = match std::fs::File::open(&path) {
                    Ok(f) => f,
                    Err(_) => { continue; }
                };
                let mut header = [0u8; 4];
                file.read_exact(&mut header).is_ok_and(|_| is_valid_programmer_magic(&header))
            };
            out.push(ProgrammerCandidate {
                name,
                path: path.to_string_lossy().into_owned(),
                valid,
                size_bytes: meta.len(),
                match_level: MatchLevel::Unknown,
                identity: None,
            });
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Candidate scoring
// ---------------------------------------------------------------------------

/// Normalize a path for case-insensitive comparison on Windows.
fn normalize_path(p: &str) -> String {
    p.to_lowercase().replace('\\', "/")
}

/// Normalize a raw hex-encoded HWID (from identify()) to bkerler format.
///
/// identify() returns `hex::encode(raw_sahara_bytes)` e.g. `"00007200e1500a00"`
/// bkerler format is LE u64 as hex: `"000a50e100720000"` (same bytes, different order).
///
/// Takes first 16 chars (8 bytes) if input is longer (Sahara may return repeated data).
fn normalize_hwid_to_bkerler(raw_hex: &str) -> String {
    let hex_str = raw_hex.to_lowercase();
    // Take first 16 chars (8 bytes) — some devices repeat the HWID
    let first16 = if hex_str.len() >= 16 { &hex_str[..16] } else { &hex_str };

    // Decode hex to bytes, read as LE u64, format back as hex
    if let Ok(bytes) = hex::decode(first16) {
        if bytes.len() >= 8 {
            let val = u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3],
                bytes[4], bytes[5], bytes[6], bytes[7],
            ]);
            return format!("{:016x}", val);
        }
    }

    // Fallback: return as-is
    hex_str
}

/// Score scanned candidates against the connected device's identity.
///
/// Uses three signals (checked in priority order, first match wins):
/// 1. DB exact match — candidate path matches a DB entry for this device's HWID:PKHash
/// 2. Filename match — filename contains device HWID or PKHash prefix (bkerler convention)
/// 3. DB other-device — candidate path matches a DB entry for a different device
///
/// After scoring, candidates are sorted: DbExact first, DbOtherDevice last.
pub fn score_candidates(
    candidates: &mut [ProgrammerCandidate],
    db: &ProgrammerDatabase,
    hwid: &str,
    pkhash: &str,
) {
    let device_key = ProgrammerDatabase::make_key(hwid, pkhash);

    // Build path sets from DB entries
    let mut device_paths: HashSet<String> = HashSet::new();
    let mut other_paths: HashSet<String> = HashSet::new();

    for (key, entry) in &db.entries {
        let norm = normalize_path(&entry.programmer_path);
        if *key == device_key {
            device_paths.insert(norm);
        } else {
            other_paths.insert(norm);
        }
    }

    // Prepare filename matching values
    let hwid_lower = hwid.to_lowercase();
    let pkhash_lower = pkhash.to_lowercase();
    let pkhash_prefix: &str = if pkhash_lower.len() >= PKHASH_PREFIX_LEN {
        &pkhash_lower[..PKHASH_PREFIX_LEN]
    } else {
        &pkhash_lower
    };

    for candidate in candidates.iter_mut() {
        let norm_path = normalize_path(&candidate.path);

        // Priority 0: DB exact match — previously used with this device (proven)
        if device_paths.contains(&norm_path) {
            candidate.match_level = MatchLevel::DbExact;
            continue;
        }

        // Priority 1: Binary verified — parsed identity matches device (cryptographic)
        if let Some(ref identity) = candidate.identity {
            let id_pkhash = identity.pk_hash.to_lowercase();
            let id_prefix = if id_pkhash.len() >= PKHASH_PREFIX_LEN {
                &id_pkhash[..PKHASH_PREFIX_LEN]
            } else {
                &id_pkhash
            };
            let pkhash_matches = id_prefix == pkhash_prefix;

            // HWID comparison: binary may have empty HWID (metadata_size=0 in some binaries).
            // PKHash match alone is sufficient — it's the cryptographic verification.
            // If HWID is available, also compare (try both raw and normalized byte order).
            let hwid_ok = if identity.hw_id.is_empty() {
                true // PKHash-only match is valid
            } else {
                let id_hw = identity.hw_id.to_lowercase();
                // Try direct match first (same format)
                id_hw == hwid_lower
                // Then try normalized (identify() raw bytes → bkerler format)
                || id_hw == normalize_hwid_to_bkerler(hwid)
            };

            if hwid_ok && pkhash_matches {
                candidate.match_level = MatchLevel::BinaryVerified;
                continue;
            }
        }

        // Priority 2: Filename pattern match
        let filename_lower = candidate.name.to_lowercase();
        let hwid_match = hwid_lower.len() >= 8 && filename_lower.contains(&hwid_lower);
        let pkhash_match = pkhash_prefix.len() >= 8 && filename_lower.contains(pkhash_prefix);

        if hwid_match || pkhash_match {
            candidate.match_level = MatchLevel::FilenameMatch;
            continue;
        }

        // Priority 3: DB other-device match
        if other_paths.contains(&norm_path) {
            candidate.match_level = MatchLevel::DbOtherDevice;
        }
        // else: stays MatchLevel::Unknown (default)
    }

    // Sort: match_level ascending (DbExact first), then name case-insensitive
    candidates.sort_by(|a, b| {
        a.match_level
            .cmp(&b.match_level)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::edl_mbn::{ELF_MAGIC, MBN_SBL_MAGIC};
    use std::io::Write;
    use tempfile::tempdir;

    fn sample_entry() -> ProgrammerEntry {
        ProgrammerEntry {
            programmer_path: "/fw/prog_firehose.elf".into(),
            programmer_name: "prog_firehose.elf".into(),
            device_serial: Some("ABC123".into()),
            storage_type: Some("ufs".into()),
            last_used: "2026-04-10T12:00:00Z".into(),
            use_count: 1,
            file_exists: false,
        }
    }

    // 1. Missing file returns empty database
    #[test]
    fn test_load_missing_file_returns_empty() {
        let dir = tempdir().unwrap();
        let db = ProgrammerDatabase::load(dir.path());
        assert!(db.entries.is_empty());
        assert!(db.is_loaded());
    }

    // 2. Save and reload round-trip
    #[test]
    fn test_save_and_reload() {
        let dir = tempdir().unwrap();
        let mut db = ProgrammerDatabase::load(dir.path());
        db.add("AABB", "CCDD", sample_entry());
        db.save().unwrap();

        let db2 = ProgrammerDatabase::load(dir.path());
        assert_eq!(db2.entries.len(), 1);
        let key = ProgrammerDatabase::make_key("aabb", "ccdd");
        let entry = db2.lookup_by_key(&key).unwrap();
        assert_eq!(entry.programmer_name, "prog_firehose.elf");
        assert_eq!(entry.use_count, 1);
    }

    // 3. Corrupt file returns empty database
    #[test]
    fn test_load_corrupt_file_returns_empty() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join(DB_FILENAME);
        std::fs::write(&db_path, "NOT_JSON{{{").unwrap();

        let db = ProgrammerDatabase::load(dir.path());
        assert!(db.entries.is_empty());
    }

    // 4. Lookup missing key
    #[test]
    fn test_lookup_missing_key() {
        let db = ProgrammerDatabase::empty();
        let key = ProgrammerDatabase::make_key("dead", "beef");
        assert!(db.lookup_by_key(&key).is_none());
    }

    // 5. Add increments use_count on existing key
    #[test]
    fn test_add_increments_use_count() {
        let mut db = ProgrammerDatabase::empty();
        db.add("AA", "BB", sample_entry());
        let key = ProgrammerDatabase::make_key("aa", "bb");
        assert_eq!(db.lookup_by_key(&key).unwrap().use_count, 1);

        let updated = ProgrammerEntry {
            programmer_path: "/fw/prog_v2.elf".into(),
            programmer_name: "prog_v2.elf".into(),
            device_serial: Some("XYZ".into()),
            storage_type: Some("emmc".into()),
            last_used: "2026-04-10T13:00:00Z".into(),
            use_count: 1,
            file_exists: false,
        };
        db.add("AA", "BB", updated);
        let entry = db.lookup_by_key(&key).unwrap();
        assert_eq!(entry.use_count, 2);
        assert_eq!(entry.programmer_name, "prog_v2.elf");
    }

    // 6. Remove entry
    #[test]
    fn test_remove_entry() {
        let mut db = ProgrammerDatabase::empty();
        db.add("AA", "BB", sample_entry());
        assert!(db.remove("aa:bb"));
        assert!(db.entries.is_empty());
    }

    // 7. Remove missing key returns false
    #[test]
    fn test_remove_missing_returns_false() {
        let mut db = ProgrammerDatabase::empty();
        assert!(!db.remove("nope:nope"));
    }

    // 8. Key is always lowercase
    #[test]
    fn test_key_is_lowercase() {
        assert_eq!(
            ProgrammerDatabase::make_key("AABB", "CCDD"),
            "aabb:ccdd"
        );
        assert_eq!(
            ProgrammerDatabase::make_key("AaBb", "CcDd"),
            "aabb:ccdd"
        );
    }

    // 9. Scan empty directory
    #[test]
    fn test_scan_programmers_empty_dir() {
        let dir = tempdir().unwrap();
        let results = scan_programmers(dir.path(), None).unwrap();
        assert!(results.is_empty());
    }

    // 10. Scan finds valid ELF file
    #[test]
    fn test_scan_programmers_finds_elf() {
        let dir = tempdir().unwrap();
        let elf_path = dir.path().join("prog_firehose.elf");
        let mut f = std::fs::File::create(&elf_path).unwrap();
        // Write ELF magic + padding
        f.write_all(&ELF_MAGIC).unwrap();
        f.write_all(&[0u8; 100]).unwrap();

        let results = scan_programmers(dir.path(), None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "prog_firehose.elf");
        assert!(results[0].valid);
        assert_eq!(results[0].size_bytes, 104);
    }

    // 11. Invalid ELF marked as not valid (0xFFFFFFFF > 50)
    #[test]
    fn test_scan_programmers_invalid_elf_marked() {
        let dir = tempdir().unwrap();
        let bad_path = dir.path().join("fake.mbn");
        let mut f = std::fs::File::create(&bad_path).unwrap();
        f.write_all(&[0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
        f.write_all(&[0u8; 60]).unwrap();

        let results = scan_programmers(dir.path(), None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].valid);
    }

    // 12. Passing a file path (not directory) returns error
    #[test]
    fn test_scan_programmers_not_dir() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("somefile.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let result = scan_programmers(&file_path, None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Not a directory"));
    }

    // 13. Scan finds .mbn with SBL magic in subdirectory
    #[test]
    fn test_scan_programmers_nested() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("qualcomm");
        std::fs::create_dir(&sub).unwrap();

        let mbn_path = sub.join("prog_emmc_firehose.mbn");
        let mut f = std::fs::File::create(&mbn_path).unwrap();
        // Write SBL magic (little-endian) + padding
        f.write_all(&MBN_SBL_MAGIC.to_le_bytes()).unwrap();
        f.write_all(&[0u8; 100]).unwrap();

        let results = scan_programmers(dir.path(), None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "prog_emmc_firehose.mbn");
        assert!(results[0].valid);
    }

    // 14. Scan finds valid .bin file (ELF magic)
    #[test]
    fn test_scan_programmers_finds_bin() {
        let dir = tempdir().unwrap();
        let bin_path = dir.path().join("prog_firehose_ddr.bin");
        let mut f = std::fs::File::create(&bin_path).unwrap();
        // Write ELF magic + padding
        f.write_all(&ELF_MAGIC).unwrap();
        f.write_all(&[0u8; 100]).unwrap();

        let results = scan_programmers(dir.path(), None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "prog_firehose_ddr.bin");
        assert!(results[0].valid);
    }

    // 15. lookup_by_key enriches entry with live file_exists flag
    #[test]
    fn test_lookup_returns_file_exists() {
        let dir = tempdir().unwrap();
        let prog = dir.path().join("prog.elf");
        std::fs::write(&prog, b"\x7fELF").unwrap();

        let mut db = ProgrammerDatabase::load(dir.path());
        let entry = ProgrammerEntry {
            programmer_path: prog.to_string_lossy().to_string(),
            programmer_name: "prog.elf".into(),
            device_serial: None,
            storage_type: None,
            last_used: "2026-04-10".into(),
            use_count: 1,
            file_exists: false, // will be enriched by lookup_by_key
        };
        let key = ProgrammerDatabase::make_key("aabbccdd", "11223344");
        db.entries.insert(key.clone(), entry);

        let found = db.lookup_by_key(&key).unwrap();
        assert!(found.file_exists);

        // Delete the file — lookup_by_key must reflect the new state.
        std::fs::remove_file(&prog).unwrap();
        let found = db.lookup_by_key(&key).unwrap();
        assert!(!found.file_exists);
    }

    // -----------------------------------------------------------------------
    // score_candidates tests
    // -----------------------------------------------------------------------

    fn make_candidate(name: &str, path: &str) -> ProgrammerCandidate {
        ProgrammerCandidate {
            name: name.into(),
            path: path.into(),
            valid: true,
            size_bytes: 1024,
            match_level: MatchLevel::Unknown,
            identity: None,
        }
    }

    // 16. Empty DB, no filename match → all Unknown
    #[test]
    fn test_score_empty_db_no_filename_match() {
        let db = ProgrammerDatabase::empty();
        let mut candidates = vec![
            make_candidate("prog.elf", "/fw/prog.elf"),
            make_candidate("other.mbn", "/fw/other.mbn"),
        ];
        score_candidates(&mut candidates, &db, "000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa");
        assert!(candidates.iter().all(|c| c.match_level == MatchLevel::Unknown));
    }

    // 17. DB exact match
    #[test]
    fn test_score_db_exact_match() {
        let mut db = ProgrammerDatabase::empty();
        db.add("000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa", ProgrammerEntry {
            programmer_path: "/fw/prog.elf".into(),
            programmer_name: "prog.elf".into(),
            device_serial: None,
            storage_type: None,
            last_used: "2026-04-11".into(),
            use_count: 1,
            file_exists: false,
        });

        let mut candidates = vec![
            make_candidate("prog.elf", "/fw/prog.elf"),
            make_candidate("other.mbn", "/fw/other.mbn"),
        ];
        score_candidates(&mut candidates, &db, "000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa");

        assert_eq!(candidates[0].match_level, MatchLevel::DbExact);
        assert_eq!(candidates[0].name, "prog.elf");
        assert_eq!(candidates[1].match_level, MatchLevel::Unknown);
    }

    // 18. Filename HWID match
    #[test]
    fn test_score_filename_hwid_match() {
        let db = ProgrammerDatabase::empty();
        let mut candidates = vec![
            make_candidate("000a50e100720000_fhprg_sdm855.bin", "/fw/000a50e100720000_fhprg_sdm855.bin"),
            make_candidate("prog.elf", "/fw/prog.elf"),
        ];
        score_candidates(&mut candidates, &db, "000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa");

        assert_eq!(candidates[0].match_level, MatchLevel::FilenameMatch);
        assert_eq!(candidates[1].match_level, MatchLevel::Unknown);
    }

    // 19. Filename PKHash prefix match
    #[test]
    fn test_score_filename_pkhash_match() {
        let db = ProgrammerDatabase::empty();
        let mut candidates = vec![
            make_candidate("oem_1bebe3863a6781db_prog.bin", "/fw/oem_1bebe3863a6781db_prog.bin"),
            make_candidate("prog.elf", "/fw/prog.elf"),
        ];
        score_candidates(&mut candidates, &db, "000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa");

        assert_eq!(candidates[0].match_level, MatchLevel::FilenameMatch);
        assert_eq!(candidates[1].match_level, MatchLevel::Unknown);
    }

    // 20. DB other-device match
    #[test]
    fn test_score_db_other_device() {
        let mut db = ProgrammerDatabase::empty();
        db.add("DIFFERENT_HWID1", "DIFFERENT_PKHASH1", ProgrammerEntry {
            programmer_path: "/fw/other_device.elf".into(),
            programmer_name: "other_device.elf".into(),
            device_serial: None,
            storage_type: None,
            last_used: "2026-04-11".into(),
            use_count: 1,
            file_exists: false,
        });

        let mut candidates = vec![
            make_candidate("other_device.elf", "/fw/other_device.elf"),
            make_candidate("prog.elf", "/fw/prog.elf"),
        ];
        score_candidates(&mut candidates, &db, "000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa");

        // other_device sorts last (after Unknown)
        assert_eq!(candidates[0].match_level, MatchLevel::Unknown);
        assert_eq!(candidates[1].match_level, MatchLevel::DbOtherDevice);
    }

    // 21. Priority: DbExact wins over filename match
    #[test]
    fn test_score_db_exact_wins_over_filename() {
        let mut db = ProgrammerDatabase::empty();
        db.add("000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa", ProgrammerEntry {
            programmer_path: "/fw/000a50e100720000_1bebe3863a6781db_prog.bin".into(),
            programmer_name: "000a50e100720000_1bebe3863a6781db_prog.bin".into(),
            device_serial: None,
            storage_type: None,
            last_used: "2026-04-11".into(),
            use_count: 1,
            file_exists: false,
        });

        let mut candidates = vec![
            make_candidate("000a50e100720000_1bebe3863a6781db_prog.bin", "/fw/000a50e100720000_1bebe3863a6781db_prog.bin"),
        ];
        score_candidates(&mut candidates, &db, "000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa");

        // DbExact takes priority even though filename also matches
        assert_eq!(candidates[0].match_level, MatchLevel::DbExact);
    }

    // 22. Sort order: DbExact > FilenameMatch > Unknown > DbOtherDevice
    #[test]
    fn test_score_sort_order() {
        let mut db = ProgrammerDatabase::empty();
        db.add("000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa", ProgrammerEntry {
            programmer_path: "/fw/known.elf".into(),
            programmer_name: "known.elf".into(),
            device_serial: None,
            storage_type: None,
            last_used: "2026-04-11".into(),
            use_count: 1,
            file_exists: false,
        });
        db.add("OTHERHWID1234567", "OTHERPKHASH12345678901234567890ab", ProgrammerEntry {
            programmer_path: "/fw/wrong.elf".into(),
            programmer_name: "wrong.elf".into(),
            device_serial: None,
            storage_type: None,
            last_used: "2026-04-11".into(),
            use_count: 1,
            file_exists: false,
        });

        let mut candidates = vec![
            make_candidate("wrong.elf", "/fw/wrong.elf"),
            make_candidate("mystery.mbn", "/fw/mystery.mbn"),
            make_candidate("000a50e100720000_fhprg.bin", "/fw/000a50e100720000_fhprg.bin"),
            make_candidate("known.elf", "/fw/known.elf"),
        ];
        score_candidates(&mut candidates, &db, "000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa");

        assert_eq!(candidates[0].match_level, MatchLevel::DbExact);
        assert_eq!(candidates[0].name, "known.elf");
        assert_eq!(candidates[1].match_level, MatchLevel::FilenameMatch);
        assert_eq!(candidates[1].name, "000a50e100720000_fhprg.bin");
        assert_eq!(candidates[2].match_level, MatchLevel::Unknown);
        assert_eq!(candidates[2].name, "mystery.mbn");
        assert_eq!(candidates[3].match_level, MatchLevel::DbOtherDevice);
        assert_eq!(candidates[3].name, "wrong.elf");
    }

    // 23. Case-insensitive filename matching
    #[test]
    fn test_score_filename_case_insensitive() {
        let db = ProgrammerDatabase::empty();
        let mut candidates = vec![
            make_candidate("000A50E100720000_FHPRG.BIN", "/fw/000A50E100720000_FHPRG.BIN"),
        ];
        score_candidates(&mut candidates, &db, "000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa");
        assert_eq!(candidates[0].match_level, MatchLevel::FilenameMatch);
    }

    // 24. Path normalization (Windows backslash vs forward slash)
    #[test]
    fn test_score_path_normalization() {
        let mut db = ProgrammerDatabase::empty();
        db.add("000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa", ProgrammerEntry {
            programmer_path: "C:\\fw\\prog.elf".into(),
            programmer_name: "prog.elf".into(),
            device_serial: None,
            storage_type: None,
            last_used: "2026-04-11".into(),
            use_count: 1,
            file_exists: false,
        });

        let mut candidates = vec![
            make_candidate("prog.elf", "C:/fw/prog.elf"),
        ];
        score_candidates(&mut candidates, &db, "000a50e100720000", "1bebe3863a6781dbaaaaaaaaaaaaaaaa");
        assert_eq!(candidates[0].match_level, MatchLevel::DbExact);
    }

    // 25. Short PKHash (< 8 chars) skips filename check
    #[test]
    fn test_score_short_pkhash_no_panic() {
        let db = ProgrammerDatabase::empty();
        let mut candidates = vec![
            make_candidate("000a50e100720000_ab_prog.bin", "/fw/000a50e100720000_ab_prog.bin"),
        ];
        // PKHash is only 4 chars — too short for filename matching, but HWID still matches
        score_candidates(&mut candidates, &db, "000a50e100720000", "abcd");
        assert_eq!(candidates[0].match_level, MatchLevel::FilenameMatch); // matched via HWID
    }

    // 26. BinaryVerified: parsed identity matches device HWID + PKHash prefix
    #[test]
    fn test_score_binary_verified_matches() {
        let db = ProgrammerDatabase::empty();
        let mut candidates = vec![ProgrammerCandidate {
            name: "prog_firehose.elf".to_string(),
            path: "/fw/prog_firehose.elf".to_string(),
            valid: true,
            size_bytes: 1024,
            match_level: MatchLevel::Unknown,
            identity: Some(crate::types::ProgrammerIdentity {
                hw_id: "000a50e101920000".to_string(),
                pk_hash: "afca69d4235117e5bfc21467068b20df85e0115d7413d5821883a6d244961581".to_string(),
                hash_algorithm: crate::types::HashAlgorithm::Sha256,
                msm_id: 0x0A50E1,
                oem_id: 0x0192,
                model_id: 0x0000,
                chipset: Some("SM8150 (SDM855)".to_string()),
                hwid_from_filename: false,
            }),
        }];
        score_candidates(&mut candidates, &db, "000a50e101920000", "afca69d4235117e5bfc21467068b20df85e0115d7413d5821883a6d244961581");
        assert_eq!(candidates[0].match_level, MatchLevel::BinaryVerified);
    }

    // 27. BinaryVerified sorts above DbExact
    #[test]
    fn test_score_db_exact_sorts_above_binary_verified() {
        let mut db = ProgrammerDatabase::empty();
        db.add("000a50e101920000", "afca69d4235117e5bfc21467068b20df85e0115d7413d5821883a6d244961581", ProgrammerEntry {
            programmer_path: "/fw/db_prog.elf".into(),
            programmer_name: "db_prog.elf".into(),
            device_serial: None,
            storage_type: None,
            last_used: "2026-04-12".into(),
            use_count: 1,
            file_exists: true,
        });
        let mut candidates = vec![
            ProgrammerCandidate {
                name: "db_prog.elf".to_string(),
                path: "/fw/db_prog.elf".to_string(),
                valid: true,
                size_bytes: 1024,
                match_level: MatchLevel::Unknown,
                identity: None,
            },
            ProgrammerCandidate {
                name: "verified_prog.elf".to_string(),
                path: "/fw/verified_prog.elf".to_string(),
                valid: true,
                size_bytes: 2048,
                match_level: MatchLevel::Unknown,
                identity: Some(crate::types::ProgrammerIdentity {
                    hw_id: "000a50e101920000".to_string(),
                    pk_hash: "afca69d4235117e5bfc21467068b20df85e0115d7413d5821883a6d244961581".to_string(),
                    hash_algorithm: crate::types::HashAlgorithm::Sha256,
                    msm_id: 0x0A50E1,
                    oem_id: 0x0192,
                    model_id: 0x0000,
                    chipset: Some("SM8150 (SDM855)".to_string()),
                    hwid_from_filename: false,
                }),
            },
        ];
        score_candidates(&mut candidates, &db, "000a50e101920000", "afca69d4235117e5bfc21467068b20df85e0115d7413d5821883a6d244961581");
        // DbExact (proven, previously used) sorts above BinaryVerified (theoretical)
        assert_eq!(candidates[0].match_level, MatchLevel::DbExact);
        assert_eq!(candidates[0].name, "db_prog.elf");
        assert_eq!(candidates[1].match_level, MatchLevel::BinaryVerified);
    }

    // 28. HWID mismatch in identity → not BinaryVerified
    #[test]
    fn test_score_binary_hwid_mismatch_no_binary_verified() {
        let db = ProgrammerDatabase::empty();
        let mut candidates = vec![ProgrammerCandidate {
            name: "prog.elf".to_string(),
            path: "/fw/prog.elf".to_string(),
            valid: true,
            size_bytes: 1024,
            match_level: MatchLevel::Unknown,
            identity: Some(crate::types::ProgrammerIdentity {
                hw_id: "000a50e101920000".to_string(),
                pk_hash: "afca69d4235117e5bfc21467068b20df85e0115d7413d5821883a6d244961581".to_string(),
                hash_algorithm: crate::types::HashAlgorithm::Sha256,
                msm_id: 0x0A50E1,
                oem_id: 0x0192,
                model_id: 0x0000,
                chipset: Some("SM8150 (SDM855)".to_string()),
                hwid_from_filename: false,
            }),
        }];
        // Device has DIFFERENT HWID
        score_candidates(&mut candidates, &db, "000c30e101920000", "afca69d4235117e5bfc21467068b20df85e0115d7413d5821883a6d244961581");
        assert_ne!(candidates[0].match_level, MatchLevel::BinaryVerified);
    }
}
