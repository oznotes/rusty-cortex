//! Qualcomm programmer binary parser.
//!
//! Extracts HWID, PKHash (SHA-256 or SHA-384), and chipset identity from
//! ELF/MBN programmer files by parsing hash segment metadata, DER certificate
//! chain walking, and signature algorithm OID detection.
//!
//! Chipset identification: 262+ msmids + 46 sochw entries from bkerler/edl.
//! Hash algorithm: OID byte scanning — sha256WithRSAEncryption → SHA-256,
//! RSASSA-PSS / ECDSA-with-SHA384 → SHA-384.
//!
//! Reference: bkerler/edl fhloaderparse.py, qualcomm_config.py, loader_db.py

use std::path::Path;

use sha2::{Sha256, Sha384, Digest};
use tracing::debug;

use crate::types::{HashAlgorithm, ProgrammerIdentity};

// --- Constants ---

pub(crate) const ELF_MAGIC: [u8; 4] = [0x7F, 0x45, 0x4C, 0x46];
pub(crate) const MBN_SBL_MAGIC: u32 = 0x844B_DCD1;
pub(crate) const MBN_MAX_IMAGE_ID: u32 = 50;

/// Check whether 4 header bytes match a known programmer format (ELF or MBN).
pub(crate) fn is_valid_programmer_magic(header: &[u8; 4]) -> bool {
    if *header == ELF_MAGIC {
        return true;
    }
    let le_val = u32::from_le_bytes(*header);
    le_val == MBN_SBL_MAGIC || le_val <= MBN_MAX_IMAGE_ID
}

/// Minimum metadata version that contains hw_id/oem_id/model_id fields.
const MIN_METADATA_VERSION: u32 = 6;

// DER SEQUENCE tag
const DER_SEQUENCE_TAG: u8 = 0x30;

// Signature algorithm OID byte patterns for PKHash algorithm detection.
// sha256WithRSAEncryption (1.2.840.113549.1.1.11) — old Qualcomm (MSM89xx)
// Only this OID maps to SHA-256 PKHash. All other signature algorithms
// (RSASSA-PSS, ECDSA-with-SHA384) use SHA-384 PKHash by Qualcomm convention.
const SHA256_RSA_OID: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];

/// Detect PKHash algorithm from root certificate's signature algorithm OID.
fn detect_hash_algorithm(cert_bytes: &[u8]) -> HashAlgorithm {
    if contains_bytes(cert_bytes, SHA256_RSA_OID) {
        HashAlgorithm::Sha256
    } else {
        // RSASSA-PSS, ECDSA-with-SHA384, or unknown → SHA-384 (modern default)
        HashAlgorithm::Sha384
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// --- Chipset lookup ---

/// Map MSM_ID to human-readable chipset name.
/// Sourced from bkerler/edl qualcomm_config.py — full 262-entry msmids table.
fn chipset_name(msm_id: u32) -> Option<&'static str> {
    match msm_id {
        0x9440E1 => Some("QDF2432"),
        0x9780E1 => Some("IPQ4018"),
        0x9790E1 => Some("IPQ4019"),
        0x0160E1 => Some("QCA4020"),
        0x9D00E1 => Some("APQ8076"),
        0x08A0E1 => Some("APQ807x"),
        0x9000E1 | 0x9010E1 => Some("APQ8084"),
        0x9630E1 => Some("APQ8092"),
        0x9410E1 => Some("APQ8094"),
        0x0940E1 => Some("MSM8905"),
        0x9600E1 => Some("MSM8909"),
        0x9680E1 => Some("APQ8009"),
        0x0510E1 => Some("MSM8909w"),
        0x0520E1 => Some("APQ8009w"),
        0x0960E1 => Some("SDX24"),
        0x0970E1 => Some("SDX24M"),
        0x7050E1 => Some("MSM8916"),
        0x7060E1 => Some("APQ8016"),
        0x0560E1 => Some("MSM8917"),
        0x0860E1 => Some("MSM8920"),
        0x91B0E1 => Some("MSM8929"),
        0x04F0E1 => Some("MSM8937"),
        0x90B0E1 => Some("MSM8939"),
        0x90C0E1 => Some("APQ8036"),
        0x0500E1 => Some("APQ8037"),
        0x90D0E1 => Some("APQ8039"),
        0x9620E1 => Some("MSM8208"),
        0x06B0E1 => Some("MSM8940"),
        0x9720E1 => Some("MSM8952"),
        0x0460E1 => Some("MSM8953"),
        0x0660E1 => Some("APQ8053"),
        0x9900E1 => Some("MSM8976"),
        0x9690E1 => Some("MSM8992"),
        0x9400E1 => Some("MSM8994"),
        0x9470E1 => Some("MSM8996"),
        0x06F0E1 | 0x0630E1 => Some("MSM8996AU"),
        0x05E0E1 => Some("MSM8998_SDM835"),
        0x94B0E1 => Some("MSM9055"),
        0x7F00E1 => Some("MDM8225"),
        0x7F30E1 => Some("MDM8225M"),
        0x9730E1 => Some("MDM9206"),
        0x9530E1 => Some("MDM9245M"),
        0x9200E1 => Some("MDM9635"),
        0x04A0E1 => Some("MDM9607"),
        0x9670E1 => Some("MDM9609"),
        0x8090E1 => Some("MDM9916"),
        0x80B0E1 => Some("MDM9955"),
        0x9210E1 => Some("MDM9x35"),
        0x9500E1 => Some("MDM9x40"),
        0x9540E1 => Some("MDM9x45"),
        0x03A0E1 => Some("MDM9x50"),
        0x7F50E1 => Some("MDM9x25"),
        0x7F40E1 => Some("MDM9625"),
        0x7F10E1 => Some("MSM9225_1"),
        0x0320E1 => Some("MDM9250"),
        0x0340E1 => Some("MDM9255"),
        0x0390E1 => Some("MDM9350"),
        0x03B0E1 => Some("MDM9x55"),
        0x07D0E1 => Some("MDM9x60"),
        0x07F0E1 => Some("MDM9x65"),
        0x1280E1 => Some("fsm100xx"),
        0x1650E1 => Some("FSM10000"),
        0x1680E1 => Some("FSM10005"),
        0x1690E1 => Some("FSM10010"),
        0x16A0E1 => Some("FSM10051"),
        0x16B0E1 => Some("FSM10056"),
        0x1530E1 => Some("ipq5018"),
        0x0C50E1 => Some("sda439"),
        0x1610E1 => Some("olympic_v1"),
        0x1720E1 => Some("olympic_v1_hybrid"),
        0x1060E1 => Some("qm215"),
        0x0BE0E1 => Some("SDM429"),
        0x0BF0E1 => Some("SDM439"),
        0x09A0E1 => Some("SDM450"),
        0x0AC0E1 => Some("SDM630"),
        0x0BA0E1 => Some("SDM632"),
        0x0BB0E1 => Some("SDA632"),
        0x08C0E1 => Some("SDM660"),
        0x07B0E1 => Some("SDX50M"),
        0x0E50E1 => Some("SDX55"),
        0x0CF0E1 => Some("SDX55M"),
        0x1250E1 => Some("SA515M"),
        0x0AB0E1 => Some("QCA6290"),
        0x0D90E1 => Some("QCA6390"),
        0x1310E1 => Some("QCA6480"),
        0x12E0E1 => Some("QCA6481"),
        0x12D0E1 => Some("QCA6491"),
        0x0D70E1 => Some("QCA6595"),
        0x0D30E1 => Some("QCN7605"),
        0x0D50E1 => Some("QCN7606"),
        0x0910E1 => Some("SDM670"),
        0x0DB0E1 => Some("SDM710"),
        0x0AA0E1 => Some("QCS605"),
        0x0ED0E1 => Some("SXR1120"),
        0x0EA0E1 => Some("SXR1130"),
        0x08E0E1 => Some("SDA845"),
        0x1A60E1 => Some("WCN7850"),
        0x1A70E1 => Some("WCN7851"),
        0x1260E1 => Some("IPQ6018"),
        0x1070E1 => Some("MDM9205"),
        0x1450E1 => Some("agatti_mdm"),
        0x14F0E1 => Some("agatti"),
        0x1850E1 => Some("agatti_mdm_iot"),
        0x1860E1 => Some("qcs2290"),
        0x13F0E1 => Some("bitra_SDM"),
        0x1410E1 => Some("bitra_SDA"),
        0x1590E1 => Some("cedros"),
        0x1360E1 => Some("kamorta"),
        0x1370E1 => Some("kamorta_P"),
        0x1730E1 => Some("kamorta_IoT_modem"),
        0x1740E1 => Some("kamorta_IoT_APQ"),
        0x1C70E1 => Some("kamorta_qrb"),
        0x1B80E1 => Some("divar"),
        0x1350E1 | 0x1520E1 | 0x19E0E1 => Some("lahaina"),
        0x1A40E1 => Some("Vordonisi"),
        0x1420E1 => Some("lahaina_premier"),
        0x14A0E1 => Some("SC8280X"),
        0x14B0E1 => Some("SA8295P"),
        0x14C0E1 => Some("SA8540P"),
        0x16F0E1 => Some("mannar"),
        0x16E0E1 => Some("mannar_P"),
        0x1470E1 => Some("moselle"),
        0x10A0E1 => Some("nicobar"),
        0x1750E1 => Some("nicobar_IoT_modem"),
        0x1760E1 => Some("nicobar_IoT_APQ"),
        0x10B0E1 => Some("QCN9000"),
        0x10C0E1 => Some("QCN9001"),
        0x1150E1 => Some("QCN9002"),
        0x10D0E1 => Some("QCN9003"),
        0x10E0E1 => Some("QCN9010"),
        0x10F0E1 => Some("QCN9011"),
        0x1110E1 => Some("QCN9012"),
        0x1140E1 => Some("QCN9013"),
        0x0E30E1 => Some("qcs401"),
        0x0E40E1 => Some("qcs403"),
        0x1040E1 => Some("qcs404"),
        0x0AF0E1 => Some("qcs405"),
        0x0EB0E1 => Some("qcs407"),
        0x0400E1 => Some("rennell_cb"),
        0x12A0E1 => Some("rennell"),
        0x12B0E1 => Some("rennell_premier"),
        0x1490E1 => Some("rennell_v1.1"),
        0x1630E1 => Some("sd7250"),
        0x11E0E1 | 0x1430E1 => Some("saipan"),
        0x0950E1 => Some("SM6150"),
        0x0EC0E1 => Some("SM6150p"),
        0x0F50E1 => Some("SM6155"),
        0x100EE0E1 => Some("SM6155p"),
        0x000EE0E1 => Some("SA6155p"),
        0x0011C0E1 => Some("QCS610"),
        0x1011C0E1 => Some("SM6150_IoT_High"),
        0x001290E1 => Some("SM6150_IoT_Low"),
        0x0E60E1 => Some("SM7150"),
        0x0A50E1 => Some("SM8150"),
        0x0A60E1 => Some("SM8150p"),
        0x0CB0E1 => Some("SDM855A"),
        0x0C30E1 | 0x0CE0E1 | 0x1560E1 => Some("SM8250"),
        0x0B80E1 => Some("sc8180x"),
        0x1230E1 => Some("sa8189P"),
        0x1510E1 => Some("SA2150p"),
        0x14D0E1 => Some("SDM662"),
        0x18A0E1 => Some("fraser"),
        0x1920E1 => Some("sm7325"),
        0x1930E1 => Some("sc7280"),
        0x1940E1 => Some("sc7295"),
        0x18B0E1 => Some("qtang2"),
        0x12C0E1 => Some("sc7180"),
        0x1A90E1 => Some("strait"),
        0x0B70E1 => Some("SDM850"),
        0x0E70E1 => Some("SM7150p"),
        0x0E80E1 => Some("SA8155"),
        0x0E90E1 => Some("SA8155p"),
        0x1440E1 => Some("chitwan"),
        0x6220E1 => Some("MSM7227A"),
        0x8040E1 => Some("APQ8026"),
        0x0550E1 => Some("APQ8017"),
        0x90F0E1 => Some("APQ8037"),
        0x9770E1 => Some("APQ8052"),
        0x9F00E1 => Some("APQ8056"),
        0x9120E1 => Some("APQ8062"),
        0x7190E1 => Some("APQ8064"),
        0x9300E1 => Some("APQ8092"),
        0x0640E1 => Some("APQ8096SG"),
        0x0620E1 => Some("APQ8098"),
        0x8110E1 => Some("MSM8210"),
        0x8140E1 => Some("MSM8212"),
        0x0590E1 => Some("MSM8217"),
        0x7BE0E1 => Some("MSM8274_AA"),
        0x8120E1 => Some("MSM8610"),
        0x8160E1 => Some("MSM8112"),
        0x8170E1 => Some("MSM8510"),
        0x8100E1 => Some("MSM8110"),
        0x8130E1 => Some("MSM8810"),
        0x8080E1 => Some("MSM8512"),
        0x8150E1 => Some("MSM8612"),
        0x8010E1 => Some("MSM8626"),
        0x8050E1 => Some("MSM8926"),
        0x9180E1 => Some("MSM8928"),
        0x9170E1 => Some("MSM8628"),
        0x7210E1 => Some("MSM8930"),
        0x72C0E1 => Some("MSM8960"),
        0x9B00E1 => Some("MSM8956"),
        0x9100E1 => Some("MSM8962"),
        0x7B00E1 => Some("MSM8974"),
        0x7BD0E1 => Some("MSM8674_AA"),
        0x7B30E1 => Some("APQ8074"),
        0x7B40E1 => Some("MSM8974AB"),
        0x7B80E1 => Some("MSM8974Pro"),
        0x7BC0E1 => Some("MSM8974ABv3"),
        0x6B10E1 => Some("MSM8974AC"),
        0x05F0E1 => Some("MSM8996Pro"),
        0x06C0E1 => Some("MSM8997"),
        0x0480E1 => Some("MDM9207"),
        0x0CC0E1 => Some("SDM636"),
        0x0930E1 => Some("SDA670"),
        0x08B0E1 => Some("SDM845"),
        0x1970E1 => Some("qcm6490"),
        0x1980E1 => Some("qcs6490"),
        0x9820E1 => Some("msm8976"),
        0x8060E1 => Some("msm8326"),
        0x9640E1 => Some("msm8992"),
        0x7B50E1 => Some("msm8674_pro"),
        0x80D0E1 => Some("fsm9915"),
        0x9110E1 => Some("msm8262"),
        0x0BC0E1 => Some("sda630"),
        0x0F20E1 => Some("sa4155p"),
        0x0EF0E1 => Some("sdm660"),
        0x8030E1 => Some("msm8126"),
        0x9130E1 => Some("apq8028"),
        0x0B90E1 => Some("sda450"),
        0x05A0E1 => Some("msm8617"),
        0x13D0E1 => Some("qcm2150"),
        0x8020E1 => Some("msm8526"),
        0x80A0E1 => Some("fsm9965"),
        0x80F0E1 => Some("fsm9900"),
        0x9140E1 => Some("msm8128"),
        0x9160E1 => Some("msm8528"),
        0x08F0E1 => Some("sdm830"),
        0x09D0E1 => Some("sda658"),
        0x08D0E1 => Some("sdm658"),
        0x9830E1 => Some("apq8076"),
        0x80C0E1 => Some("fsm9950"),
        0x80E0E1 => Some("fsm9910"),
        0x15A0E1 => Some("qrb516"),
        0x8000E1 => Some("msm8226"),
        0x9D70E1 => Some("msm8229"),
        0x90E0E1 => Some("msm8236"),
        0x9660E1 => Some("mdm9309"),
        0x04E0E1 => Some("apq8096au"),
        0x9570E1 => Some("msm8239"),
        0x1990E1 => Some("OlympicLE"),
        0x0DA0E1 => Some("sc8180xp"),
        // Legacy MSM8x30/MSM8x60 family (Nokia WP8 era, Qualcomm factory loaders)
        0x006B00E1 => Some("MSM8230"),
        0x006B40E1 => Some("MSM8630"),
        0x006B50E1 => Some("MSM8230AB"),
        0x007150E1 => Some("MSM8130"),
        0x007200E1 => Some("APQ8030"),
        0x007220E1 => Some("APQ8030AA"),
        // Legacy MSM8974 family pre-production IDs (Nokia Foxconn loaders)
        0x007940E1 => Some("APQ8074"),
        0x007980E1 => Some("APQ8074AA"),
        0x007D00E1 => Some("MSM8974AA"),
        0x007D10E1 => Some("MSM8974AAv2"),
        // MSM8x74 bridge era
        0x007530E1 => Some("MSM8x74"),
        0x007E10E1 => Some("APQ8074AB"),
        // Legacy MSM8x10/MSM8x12 family
        0x008910E1 => Some("MSM8x10"),
        0x008920E1 => Some("MSM8x12"),
        0x008A20E1 => Some("APQ8x12"),
        // MSM8936/8939 era
        0x008F10E1 => Some("MSM8936"),
        // MSM8226 extended family
        0x009150E1 => Some("MSM8x26"),
        // MDM9x50/MDM9x55 era
        0x009510E1 => Some("MDM9x55"),
        // Light Phone
        0x0003E0E1 => Some("MSM8x37"),
        // Unknown legacy ID (seen in Nokia collection)
        0x001870E1 => Some("MSM8x39"),
        _ => None,
    }
}

/// Map SOC_HW ID to human-readable chipset name.
/// Sourced from bkerler/edl qualcomm_config.py:332-380.
/// Returns the first/primary name when multiple chips share a SOC_HW ID.
fn sochw_name(soc_hw: u16) -> Option<&'static str> {
    match soc_hw {
        0x2013 => Some("MDM9205"),
        0x2014 => Some("qcs405"),
        0x2017 => Some("IPQ6018"),
        0x3002 => Some("MSM8998_SDM835"),
        0x3006 => Some("SDM660"),
        0x3007 => Some("SDM630"),
        0x4003 => Some("QCA4020"),
        0x4004 => Some("IPQ8074"),
        0x400A => Some("QCA6390"),
        0x400B => Some("QCN7605"),
        0x400D => Some("QCN9000"),
        0x4014 => Some("moselle"),
        0x4017 => Some("WCN7850"),
        0x6000 => Some("SDM845"),
        0x6001 => Some("SDA845"),
        0x6002 => Some("SDX24"),
        0x6003 => Some("SM8150"),
        0x6004 => Some("SDM670"),
        0x6005 => Some("SDM670"),
        0x6006 => Some("sc8180x"),
        0x6007 => Some("SM6150"),
        0x6008 => Some("SM8250"),
        0x6009 => Some("SDM670"),
        0x600B => Some("SDX55"),
        0x600C => Some("SM7150"),
        0x600D => Some("saipan"),
        0x600E => Some("rennell"),
        0x600F => Some("lahaina"),
        0x6012 => Some("bitra_SDM"),
        0x6013 => Some("chitwan"),
        0x6014 => Some("SC8280X"),
        0x6016 => Some("olympic_v1"),
        0x6017 => Some("cedros"),
        0x6018 => Some("sm7325"),
        0x7001 => Some("qtang2"),
        0x7200 => Some("SDM662"),
        0x9001 => Some("nicobar"),
        0x9002 => Some("kamorta"),
        0x9003 => Some("agatti"),
        0x9004 => Some("mannar"),
        0x9006 => Some("strait"),
        0x9007 => Some("divar"),
        _ => None,
    }
}

/// Resolve chipset name with multi-strategy fallback.
/// Implements bkerler's convertmsmid() logic (loader_db.py:75-89).
///
/// Strategy:
/// 1. Direct msmids lookup
/// 2. SOC_HW conversion: if low byte != 0xE1 and nonzero, extract upper 16 bits → sochw lookup
/// 3. Byte-order normalization for format mismatches
pub fn resolve_chipset(msm_id: u32) -> Option<&'static str> {
    if msm_id == 0 {
        return None;
    }

    // 1. Direct lookup in msmids table
    if let Some(name) = chipset_name(msm_id) {
        return Some(name);
    }

    // 2. SOC_HW conversion (bkerler convertmsmid logic)
    // If low byte is NOT 0xE1, this might be a SOC_HW version register value
    if (msm_id & 0xFF) != 0xE1 {
        let soc_hw = (msm_id >> 16) as u16;
        if soc_hw != 0 {
            if let Some(name) = sochw_name(soc_hw) {
                return Some(name);
            }
        }
    }

    // 3. Try byte-order normalization for non-E1 IDs
    // Some metadata stores MSM_ID with trailing 0x00 instead of 0xE1
    if (msm_id & 0xFF) == 0x00 && msm_id <= 0xFFFFFF {
        let shifted = (msm_id >> 8) | 0xE1;
        if let Some(name) = chipset_name(shifted) {
            return Some(name);
        }
    }

    None
}

/// Parse a Qualcomm programmer binary (ELF or MBN) and extract identity.
///
/// Returns `None` on any parse failure — never panics.
/// If binary metadata is missing (metadata_size=0), falls back to extracting
/// HWID from filename using bkerler convention: `{hwid}_{pkhash}_fhprg.{ext}`.
pub fn parse_programmer_identity(path: &Path) -> Option<ProgrammerIdentity> {
    // Size guard — programmer files are typically 1-5MB, never >50MB
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > 50 * 1024 * 1024 {
        return None;
    }

    let data = std::fs::read(path).ok()?;
    let mut identity = parse_programmer_identity_from_bytes(&data)?;

    // If binary metadata didn't provide HWID, try extracting from filename
    // bkerler convention: {hwid}_{pkhash}_fhprg.{ext}
    if identity.hw_id.is_empty() {
        if let Some(fname) = path.file_name().and_then(|f| f.to_str()) {
            enrich_from_filename(fname, &mut identity);
        }
    }

    Some(identity)
}

/// Try to extract HWID fields from bkerler-style filename.
/// Format: `{hwid}_{pkhash}_{suffix}.{ext}` where hwid is 16 hex chars.
fn enrich_from_filename(filename: &str, identity: &mut ProgrammerIdentity) -> Option<()> {
    let stem = filename.rsplit('.').next_back().unwrap_or(filename);
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() < 2 {
        return None;
    }

    let hwid_str = parts[0];
    // HWID should be 16 hex chars (8 bytes)
    if hwid_str.len() != 16 || !hwid_str.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    // Parse HWID in bkerler format: {msm_id:8}{oem_id:4}{model_id:4}
    let msm_id = u32::from_str_radix(&hwid_str[..8], 16).ok()?;
    // bkerler: msm_id = int(hwidstr[2:8], 16) — skip first 2 chars (usually "00")
    let msm_id_inner = u32::from_str_radix(&hwid_str[2..8], 16).ok()?;
    let oem_id = u16::from_str_radix(&hwid_str[8..12], 16).ok()?;
    let model_id = u16::from_str_radix(&hwid_str[12..16], 16).ok()?;

    identity.hw_id = hwid_str.to_lowercase();
    identity.oem_id = oem_id;
    identity.model_id = model_id;
    identity.hwid_from_filename = true;

    // Try msm_id_inner first (bkerler convention: skip leading "00" bytes).
    // If that doesn't resolve, fall back to the full 4-byte value which covers
    // SOC_HW-style IDs stored as e.g. 0x30060000 (upper 16 bits = SOC_HW 0x3006).
    let resolved_id = if msm_id_inner > 0 && resolve_chipset(msm_id_inner).is_some() {
        msm_id_inner
    } else if resolve_chipset(msm_id).is_some() {
        msm_id
    } else if msm_id_inner > 0 {
        msm_id_inner // keep inner even if unresolved (it's the canonical HWID)
    } else {
        msm_id
    };
    identity.msm_id = resolved_id;
    identity.chipset = resolve_chipset(resolved_id).map(|s| s.to_string());

    Some(())
}

/// Parse identity from in-memory bytes (testable without filesystem).
fn parse_programmer_identity_from_bytes(data: &[u8]) -> Option<ProgrammerIdentity> {
    if data.len() < 4 {
        return None;
    }

    let magic = [data[0], data[1], data[2], data[3]];

    if magic == ELF_MAGIC {
        parse_elf_identity(data)
    } else {
        let le_val = u32::from_le_bytes(magic);
        if le_val == MBN_SBL_MAGIC || le_val <= MBN_MAX_IMAGE_ID {
            parse_mbn_identity(data)
        } else {
            None
        }
    }
}

/// Parse identity from an ELF programmer binary.
fn parse_elf_identity(data: &[u8]) -> Option<ProgrammerIdentity> {
    if data.len() < 64 {
        return None;
    }

    let elf_class = data[4]; // 1 = 32-bit, 2 = 64-bit

    let (e_phoff, e_phentsize, e_phnum) = match elf_class {
        1 => {
            // 32-bit ELF header
            if data.len() < 52 { return None; }
            let phoff = u32::from_le_bytes(data[28..32].try_into().ok()?) as usize;
            let phentsize = u16::from_le_bytes(data[42..44].try_into().ok()?) as usize;
            let phnum = u16::from_le_bytes(data[44..46].try_into().ok()?) as usize;
            (phoff, phentsize, phnum)
        }
        2 => {
            // 64-bit ELF header
            if data.len() < 64 { return None; }
            let phoff = u64::from_le_bytes(data[32..40].try_into().ok()?) as usize;
            let phentsize = u16::from_le_bytes(data[54..56].try_into().ok()?) as usize;
            let phnum = u16::from_le_bytes(data[56..58].try_into().ok()?) as usize;
            (phoff, phentsize, phnum)
        }
        _ => return None,
    };

    // Validate program header table fits in file
    let ph_end = e_phoff.checked_add(e_phnum.checked_mul(e_phentsize)?)?;
    if ph_end > data.len() || e_phnum == 0 {
        return None;
    }

    // Find the hash segment — PT_NULL with nonzero file size and nonzero offset,
    // or segment with hash flag (p_flags & 0x02000000).
    // PH[0] at offset 0 is the ELF/phdr descriptor, not the hash segment — skip it.
    // The real hash segment is typically PH[1] with a nonzero file offset.
    let mut hash_offset: Option<usize> = None;
    let mut hash_size: Option<usize> = None;

    for i in 0..e_phnum {
        let ph_start = e_phoff + i * e_phentsize;
        if ph_start + e_phentsize > data.len() {
            break;
        }

        let (p_type, p_offset, p_filesz, p_flags) = match elf_class {
            1 => {
                let t = u32::from_le_bytes(data[ph_start..ph_start + 4].try_into().ok()?);
                let o = u32::from_le_bytes(data[ph_start + 4..ph_start + 8].try_into().ok()?) as usize;
                let s = u32::from_le_bytes(data[ph_start + 16..ph_start + 20].try_into().ok()?) as usize;
                let f = u32::from_le_bytes(data[ph_start + 24..ph_start + 28].try_into().ok()?);
                (t, o, s, f)
            }
            2 => {
                let t = u32::from_le_bytes(data[ph_start..ph_start + 4].try_into().ok()?);
                let o = u64::from_le_bytes(data[ph_start + 8..ph_start + 16].try_into().ok()?) as usize;
                let s = u64::from_le_bytes(data[ph_start + 32..ph_start + 40].try_into().ok()?) as usize;
                let f = u32::from_le_bytes(data[ph_start + 4..ph_start + 8].try_into().ok()?);
                (t, o, s, f)
            }
            _ => return None,
        };

        // Hash segment: must have nonzero offset (offset 0 = ELF header, not hash data),
        // and either be PT_NULL with data or have the Qualcomm hash flag.
        if p_offset > 0
            && p_filesz > 0
            && ((p_type == 0) || (p_flags & 0x0200_0000 != 0))
            && p_offset + p_filesz <= data.len()
        {
            hash_offset = Some(p_offset);
            hash_size = Some(p_filesz);
            break;
        }
    }

    let seg_offset = hash_offset?;
    let seg_size = hash_size?;
    let segment = data.get(seg_offset..seg_offset + seg_size)?;

    extract_identity_from_hash_segment(segment)
}

/// Parse identity from an MBN programmer binary.
fn parse_mbn_identity(data: &[u8]) -> Option<ProgrammerIdentity> {
    // MBN header at offset 0x0C (after 12-byte SBL header prefix)
    // Format: 10 x u32 LE fields (40 bytes)
    if data.len() < 0x0C + 40 {
        return None;
    }

    // MBN header fields (10 x u32, starting at 0x0C):
    //   +0:  imageid, +4: version, +8: imagesrc, +12: loadaddr,
    //   +16: imagesz, +20: codesz, +24: sigptr, +28: sigsz,
    //   +32: certptr, +36: certsz
    let header_start = 0x0C;
    let code_sz = u32::from_le_bytes(data[header_start + 20..header_start + 24].try_into().ok()?) as usize;
    let _sig_ptr = u32::from_le_bytes(data[header_start + 24..header_start + 28].try_into().ok()?) as usize;
    let _sig_sz = u32::from_le_bytes(data[header_start + 28..header_start + 32].try_into().ok()?) as usize;
    let _cert_ptr = u32::from_le_bytes(data[header_start + 32..header_start + 36].try_into().ok()?) as usize;
    let cert_sz = u32::from_le_bytes(data[header_start + 36..header_start + 40].try_into().ok()?) as usize;

    // Extract PKHash from cert chain.
    // cert_ptr/sig_ptr may be virtual addresses (>> file size), not file offsets.
    // Most reliable: cert chain is always the last cert_sz bytes of the file.
    let (pk_hash, hash_algorithm) = if cert_sz > 0 && cert_sz < data.len() {
        let cert_start = data.len() - cert_sz;
        extract_pkhash_from_cert_chain(&data[cert_start..])
    } else {
        // Fallback: scan for DER certs after header + code
        let search = (header_start + 40 + code_sz).min(data.len());
        find_and_extract_pkhash(data, search)
    }?;

    // Try to extract metadata — located after header (40 bytes) + hash table (code_sz)
    let meta_offset = header_start + 40 + code_sz;

    let metadata = extract_metadata(data, meta_offset);

    if let Some((msm_id, oem_id, model_id)) = metadata {
        let chipset = resolve_chipset(msm_id).map(|s| s.to_string());
        Some(ProgrammerIdentity {
            hw_id: format!("{:08x}{:04x}{:04x}", msm_id, oem_id, model_id),
            pk_hash,
            hash_algorithm,
            msm_id,
            oem_id,
            model_id,
            chipset,
            hwid_from_filename: false,
        })
    } else {
        // PKHash-only identity
        Some(ProgrammerIdentity {
            hw_id: String::new(),
            pk_hash,
            hash_algorithm,
            msm_id: 0,
            oem_id: 0,
            model_id: 0,
            chipset: None,
            hwid_from_filename: false,
        })
    }
}

/// Size of the internal MBN header in a hash segment (10 x u32 = 40 bytes).
const HASH_SEG_HEADER_SIZE: usize = 40;

/// Extract identity from a hash segment.
///
/// The hash segment has an internal MBN header:
///   [0x00] image_id, version, image_src, load_addr, image_size,
///          code_size (hash table), sig_ptr, sig_size, cert_ptr, cert_size
///   [0x28] hash table (code_size bytes)
///   [0x28 + code_size] metadata (may be 0 bytes)
///   [...] signature (sig_size bytes)
///   [segment_end - cert_size] certificate chain (cert_size bytes)
fn extract_identity_from_hash_segment(segment: &[u8]) -> Option<ProgrammerIdentity> {
    if segment.len() < HASH_SEG_HEADER_SIZE {
        return None;
    }

    // Parse internal MBN header
    let code_size = u32::from_le_bytes(segment[0x14..0x18].try_into().ok()?) as usize;
    let sig_size = u32::from_le_bytes(segment[0x1C..0x20].try_into().ok()?) as usize;
    let cert_size = u32::from_le_bytes(segment[0x24..0x28].try_into().ok()?) as usize;

    // Locate cert chain: always at segment_end - cert_size
    let (pk_hash, hash_algorithm) = if cert_size > 0 && cert_size <= segment.len() {
        let cert_start = segment.len() - cert_size;
        extract_pkhash_from_cert_chain(&segment[cert_start..])
    } else {
        // Fallback: scan for DER SEQUENCE tags after the hash table
        let search_start = (HASH_SEG_HEADER_SIZE + code_size).min(segment.len());
        find_and_extract_pkhash(segment, search_start)
    }?;

    // Try to extract metadata from after hash table
    // metadata_size = total - header - code - sig - cert (may be 0)
    let fixed_parts = HASH_SEG_HEADER_SIZE + code_size + sig_size + cert_size;
    let metadata_size = segment.len().saturating_sub(fixed_parts);
    let metadata_offset = HASH_SEG_HEADER_SIZE + code_size;

    let metadata = if metadata_size >= 24 {
        extract_metadata(segment, metadata_offset)
    } else {
        None
    };

    if let Some((msm_id, oem_id, model_id)) = metadata {
        let chipset = resolve_chipset(msm_id).map(|s| s.to_string());
        Some(ProgrammerIdentity {
            hw_id: format!("{:08x}{:04x}{:04x}", msm_id, oem_id, model_id),
            pk_hash,
            hash_algorithm,
            msm_id,
            oem_id,
            model_id,
            chipset,
            hwid_from_filename: false,
        })
    } else {
        // PKHash-only identity — metadata not available in this binary
        Some(ProgrammerIdentity {
            hw_id: String::new(),
            pk_hash,
            hash_algorithm,
            msm_id: 0,
            oem_id: 0,
            model_id: 0,
            chipset: None,
            hwid_from_filename: false,
        })
    }
}

/// Read metadata fields from a buffer at the given offset.
/// Returns (msm_id, oem_id, model_id) or None.
fn extract_metadata(data: &[u8], offset: usize) -> Option<(u32, u16, u16)> {
    // Metadata structure (bkerler fhloaderparse.py):
    // offset+0:  major version (u32)
    // offset+4:  minor version (u32)
    // offset+8:  sw_id (u32)
    // offset+12: hw_id (u32) — this is MSM_ID
    // offset+16: oem_id (u32, but only lower 16 bits matter)
    // offset+20: model_id (u32, but only lower 16 bits matter)
    if offset + 24 > data.len() {
        return None;
    }

    let major = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);

    // Version check — modern metadata format
    if !(MIN_METADATA_VERSION..=100).contains(&major) {
        debug!("edl_mbn: metadata version {} outside expected range", major);
        return None;
    }

    let hw_id = u32::from_le_bytes(data[offset + 12..offset + 16].try_into().ok()?);
    let oem_id = u32::from_le_bytes(data[offset + 16..offset + 20].try_into().ok()?) as u16;
    let model_id = u32::from_le_bytes(data[offset + 20..offset + 24].try_into().ok()?) as u16;

    // Sanity: MSM_ID should be nonzero
    if hw_id == 0 {
        return None;
    }

    Some((hw_id, oem_id, model_id))
}

/// Search for a DER certificate chain starting from `search_start` and extract PKHash.
fn find_and_extract_pkhash(data: &[u8], search_start: usize) -> Option<(String, HashAlgorithm)> {
    for i in search_start..data.len().saturating_sub(4) {
        if data[i] == DER_SEQUENCE_TAG {
            if let Some(result) = extract_pkhash_from_cert_chain(&data[i..]) {
                return Some(result);
            }
        }
    }
    None
}

/// Walk a DER-encoded certificate chain and hash the root (last) certificate.
///
/// DER chain: concatenated SEQUENCE structures.
/// Each cert: tag 0x30 + length + content.
/// The hash algorithm is detected from the root cert's signature algorithm OID.
fn extract_pkhash_from_cert_chain(data: &[u8]) -> Option<(String, HashAlgorithm)> {
    let mut certs: Vec<&[u8]> = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        if data[pos] != DER_SEQUENCE_TAG {
            break;
        }

        let (total_len, header_len) = der_read_length(data, pos + 1)?;
        let cert_end = pos + header_len + total_len;
        if cert_end > data.len() {
            break;
        }

        certs.push(&data[pos..cert_end]);
        pos = cert_end;
    }

    if certs.is_empty() {
        return None;
    }

    let root_cert = certs.last()?;

    // Sanity: root cert should be reasonably sized (100 bytes to 10KB)
    if root_cert.len() < 100 || root_cert.len() > 10240 {
        return None;
    }

    let algorithm = detect_hash_algorithm(root_cert);
    let hash = match algorithm {
        HashAlgorithm::Sha256 => hex::encode(Sha256::digest(root_cert)),
        HashAlgorithm::Sha384 => hex::encode(Sha384::digest(root_cert)),
    };
    Some((hash, algorithm))
}

/// Parse DER length encoding at `data[offset..]`.
/// Returns (content_length, header_bytes_consumed).
fn der_read_length(data: &[u8], offset: usize) -> Option<(usize, usize)> {
    if offset >= data.len() {
        return None;
    }

    let first = data[offset];
    if first < 0x80 {
        // Short form: length is the byte itself
        Some((first as usize, 2))
    } else if first == 0x80 {
        // Indefinite length — not valid for DER
        None
    } else {
        // Long form: first byte = 0x80 | num_length_bytes
        let num_bytes = (first & 0x7F) as usize;
        if num_bytes > 4 || offset + 1 + num_bytes > data.len() {
            return None;
        }

        let mut length: usize = 0;
        for i in 0..num_bytes {
            length = length.checked_shl(8)?;
            length = length.checked_add(data[offset + 1 + i] as usize)?;
        }

        Some((length, 2 + num_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_programmer_magic() {
        assert!(is_valid_programmer_magic(&ELF_MAGIC));
        assert!(is_valid_programmer_magic(&MBN_SBL_MAGIC.to_le_bytes()));
        assert!(is_valid_programmer_magic(&13u32.to_le_bytes())); // valid image_id
        assert!(is_valid_programmer_magic(&0u32.to_le_bytes())); // image_id 0
        assert!(!is_valid_programmer_magic(&[0xFF, 0xFF, 0xFF, 0xFF])); // garbage
        assert!(!is_valid_programmer_magic(&[0x51, 0x00, 0x00, 0x00])); // 81 > MAX_IMAGE_ID
    }

    #[test]
    fn test_chipset_lookup_known() {
        assert_eq!(chipset_name(0x0A50E1), Some("SM8150"));
        assert_eq!(chipset_name(0x9470E1), Some("MSM8996"));
    }

    #[test]
    fn test_chipset_lookup_full_table() {
        assert_eq!(chipset_name(0x0A50E1), Some("SM8150"));
        assert_eq!(chipset_name(0x0C30E1), Some("SM8250"));
        assert_eq!(chipset_name(0x1350E1), Some("lahaina"));
        assert_eq!(chipset_name(0x0460E1), Some("MSM8953"));
        assert_eq!(chipset_name(0x08B0E1), Some("SDM845"));
        assert_eq!(chipset_name(0x05E0E1), Some("MSM8998_SDM835"));
        assert_eq!(chipset_name(0x9600E1), Some("MSM8909"));
        assert_eq!(chipset_name(0x7050E1), Some("MSM8916"));
        assert_eq!(chipset_name(0x0CC0E1), Some("SDM636"));
        assert_eq!(chipset_name(0x14D0E1), Some("SDM662"));
        assert_eq!(chipset_name(0x1B80E1), Some("divar"));
        assert_eq!(chipset_name(0x0DA0E1), Some("sc8180xp"));
        assert_eq!(chipset_name(0xDEADBE), None); // unknown returns None
    }

    #[test]
    fn test_sochw_lookup() {
        assert_eq!(sochw_name(0x6000), Some("SDM845"));
        assert_eq!(sochw_name(0x6003), Some("SM8150"));
        assert_eq!(sochw_name(0x6008), Some("SM8250"));
        assert_eq!(sochw_name(0x600F), Some("lahaina"));
        assert_eq!(sochw_name(0x9007), Some("divar"));
        assert_eq!(sochw_name(0xFFFF), None);
    }

    #[test]
    fn test_chipset_lookup_unknown() {
        assert_eq!(chipset_name(0xFFFFFF), None);
        assert_eq!(chipset_name(0), None);
    }

    #[test]
    fn test_parse_empty_data() {
        assert!(parse_programmer_identity_from_bytes(&[]).is_none());
    }

    #[test]
    fn test_parse_short_data() {
        assert!(parse_programmer_identity_from_bytes(&[0x7F, 0x45]).is_none());
    }

    #[test]
    fn test_parse_non_programmer() {
        let data = vec![0xFF; 512];
        assert!(parse_programmer_identity_from_bytes(&data).is_none());
    }

    #[test]
    fn test_der_read_length_short() {
        let data = [DER_SEQUENCE_TAG, 50];
        let (len, hdr) = der_read_length(&data, 1).unwrap();
        assert_eq!(len, 50);
        assert_eq!(hdr, 2);
    }

    #[test]
    fn test_der_read_length_long_2byte() {
        let data = [DER_SEQUENCE_TAG, 0x82, 0x01, 0x00];
        let (len, hdr) = der_read_length(&data, 1).unwrap();
        assert_eq!(len, 256);
        assert_eq!(hdr, 4);
    }

    #[test]
    fn test_der_read_length_indefinite_rejected() {
        let data = [DER_SEQUENCE_TAG, 0x80];
        assert!(der_read_length(&data, 1).is_none());
    }

    #[test]
    fn test_extract_pkhash_single_cert_no_oid_defaults_sha384() {
        let mut cert = vec![DER_SEQUENCE_TAG];
        let content = vec![0xAA; 200];
        cert.push(0x81); // long form, 1 length byte
        cert.push(200);
        cert.extend_from_slice(&content);

        let expected_sha384 = hex::encode(Sha384::digest(&cert));
        let (hash, algorithm) = extract_pkhash_from_cert_chain(&cert).unwrap();
        // No OID found → defaults to SHA-384 (modern standard)
        assert_eq!(algorithm, HashAlgorithm::Sha384);
        assert_eq!(hash, expected_sha384);
    }

    #[test]
    fn test_extract_pkhash_chain_takes_last() {
        let build_cert = |content_byte: u8| -> Vec<u8> {
            let mut cert = vec![DER_SEQUENCE_TAG];
            let content = vec![content_byte; 200];
            cert.push(0x81);
            cert.push(200);
            cert.extend_from_slice(&content);
            cert
        };

        let cert1 = build_cert(0xAA);
        let cert2 = build_cert(0xBB);
        let cert3 = build_cert(0xCC);

        let mut chain = Vec::new();
        chain.extend_from_slice(&cert1);
        chain.extend_from_slice(&cert2);
        chain.extend_from_slice(&cert3);

        let expected_sha384 = hex::encode(Sha384::digest(&cert3));
        let (hash, algorithm) = extract_pkhash_from_cert_chain(&chain).unwrap();
        // Root cert (last) hashed — no OID in synthetic cert defaults to SHA-384
        assert_eq!(algorithm, HashAlgorithm::Sha384);
        assert_eq!(hash, expected_sha384);
    }

    #[test]
    fn test_extract_metadata_valid() {
        let mut data = vec![0u8; 64];
        data[0..4].copy_from_slice(&7u32.to_le_bytes());
        data[4..8].copy_from_slice(&0u32.to_le_bytes());
        data[8..12].copy_from_slice(&0u32.to_le_bytes());
        data[12..16].copy_from_slice(&0x0A50E1u32.to_le_bytes());
        data[16..20].copy_from_slice(&0x0192u32.to_le_bytes());
        data[20..24].copy_from_slice(&0u32.to_le_bytes());

        let (msm_id, oem_id, model_id) = extract_metadata(&data, 0).unwrap();
        assert_eq!(msm_id, 0x0A50E1);
        assert_eq!(oem_id, 0x0192);
        assert_eq!(model_id, 0);
    }

    #[test]
    fn test_extract_metadata_old_version_rejected() {
        let mut data = vec![0u8; 64];
        data[0..4].copy_from_slice(&3u32.to_le_bytes());
        data[12..16].copy_from_slice(&0x0A50E1u32.to_le_bytes());
        assert!(extract_metadata(&data, 0).is_none());
    }

    #[test]
    fn test_extract_metadata_zero_hwid_rejected() {
        let mut data = vec![0u8; 64];
        data[0..4].copy_from_slice(&7u32.to_le_bytes());
        assert!(extract_metadata(&data, 0).is_none());
    }

    #[test]
    fn test_hwid_composition_format() {
        let msm_id: u32 = 0x0A50E1;
        let oem_id: u16 = 0x0192;
        let model_id: u16 = 0x0000;
        let hw_id = format!("{:08x}{:04x}{:04x}", msm_id, oem_id, model_id);
        assert_eq!(hw_id, "000a50e101920000");
        assert_eq!(hw_id.len(), 16);
    }

    #[test]
    fn test_parse_real_elf_programmer() {
        // Test against a real bkerler programmer file if available
        let path = std::path::Path::new(r"C:\Users\oz\Documents\GitHub\edl\edl\Loaders\oppo\prog_firehose_ddr.elf");
        if !path.exists() {
            eprintln!("Skipping real file test — file not found");
            return;
        }
        let result = parse_programmer_identity(path);
        assert!(result.is_some(), "Should parse a real ELF programmer");
        let id = result.unwrap();
        assert!(!id.pk_hash.is_empty(), "PKHash should be extracted");
        // Single hash: 64 hex chars (SHA-256) or 96 hex chars (SHA-384)
        assert!(
            id.pk_hash.len() == 64 || id.pk_hash.len() == 96,
            "PKHash should be single hash (64 or 96 hex chars), got len {}",
            id.pk_hash.len()
        );
        assert!(!id.pk_hash.contains(':'), "PKHash should not contain colon (old dual format)");
        eprintln!("  ELF pk_hash ({}): {}", id.hash_algorithm, &id.pk_hash[..32]);
        eprintln!("  ELF hw_id: {:?}", if id.hw_id.is_empty() { "none" } else { &id.hw_id });
        eprintln!("  ELF chipset: {:?}", id.chipset);
    }

    #[test]
    fn test_parse_real_mbn_programmer() {
        // Test against a real MBN (SBL magic) programmer file if available
        let path = std::path::Path::new(r"C:\Users\oz\Documents\GitHub\edl\edl\Loaders\amazon\007b30e100000000_9ad772705cab4511_fhprg.bin");
        if !path.exists() {
            eprintln!("Skipping real file test — file not found");
            return;
        }
        let result = parse_programmer_identity(path);
        assert!(result.is_some(), "Should parse a real MBN programmer");
        let id = result.unwrap();
        assert!(!id.pk_hash.is_empty(), "PKHash should be extracted");
        // Single hash: 64 hex chars (SHA-256) or 96 hex chars (SHA-384)
        assert!(
            id.pk_hash.len() == 64 || id.pk_hash.len() == 96,
            "PKHash should be single hash (64 or 96 hex chars), got len {}",
            id.pk_hash.len()
        );
        assert!(!id.pk_hash.contains(':'), "PKHash should not contain colon (old dual format)");
        eprintln!("  MBN pk_hash ({}): {}", id.hash_algorithm, &id.pk_hash[..32]);
        eprintln!("  MBN hw_id: {:?}", if id.hw_id.is_empty() { "none" } else { &id.hw_id });
        eprintln!("  MBN chipset: {:?}", id.chipset);
    }

    #[test]
    fn test_resolve_chipset_direct() {
        assert_eq!(resolve_chipset(0x0A50E1), Some("SM8150"));
        assert_eq!(resolve_chipset(0x08B0E1), Some("SDM845"));
    }

    #[test]
    fn test_resolve_chipset_sochw() {
        // SOC_HW: 0x60000000 → upper 16 bits = 0x6000 → "SDM845"
        assert_eq!(resolve_chipset(0x60000000), Some("SDM845"));
        assert_eq!(resolve_chipset(0x60030100), Some("SM8150"));
        assert_eq!(resolve_chipset(0x60080100), Some("SM8250"));
    }

    #[test]
    fn test_resolve_chipset_unknown() {
        assert_eq!(resolve_chipset(0xDEADBEEF), None);
        assert_eq!(resolve_chipset(0), None);
    }

    #[test]
    fn test_detect_hash_sha256_rsa() {
        let mut cert = vec![0u8; 200];
        cert[50..50 + SHA256_RSA_OID.len()].copy_from_slice(SHA256_RSA_OID);
        assert_eq!(detect_hash_algorithm(&cert), HashAlgorithm::Sha256);
    }

    #[test]
    fn test_detect_hash_sha384_no_sha256_oid() {
        // Any cert without sha256WithRSAEncryption → SHA-384 (modern default)
        let cert = vec![0xAA; 200];
        assert_eq!(detect_hash_algorithm(&cert), HashAlgorithm::Sha384);
    }

    #[test]
    fn test_detect_hash_unknown_defaults_sha384() {
        let cert = vec![0u8; 200];
        assert_eq!(detect_hash_algorithm(&cert), HashAlgorithm::Sha384);
    }

    #[test]
    fn test_contains_bytes_found() {
        let haystack = [0x00, 0x01, 0x02, 0x03, 0x04];
        assert!(contains_bytes(&haystack, &[0x01, 0x02, 0x03]));
    }

    #[test]
    fn test_contains_bytes_not_found() {
        let haystack = [0x00, 0x01, 0x02, 0x03, 0x04];
        assert!(!contains_bytes(&haystack, &[0x01, 0x03]));
    }

    #[test]
    fn test_extract_pkhash_sha256_cert() {
        // Build a cert that contains the SHA-256 RSA OID
        let mut content = vec![0xAA; 200];
        content[50..50 + SHA256_RSA_OID.len()].copy_from_slice(SHA256_RSA_OID);

        let mut cert = vec![DER_SEQUENCE_TAG];
        cert.push(0x81); // long form, 1 length byte
        cert.push(200);
        cert.extend_from_slice(&content);

        let expected_sha256 = hex::encode(Sha256::digest(&cert));
        let (hash, algorithm) = extract_pkhash_from_cert_chain(&cert).unwrap();
        assert_eq!(algorithm, HashAlgorithm::Sha256);
        assert_eq!(hash, expected_sha256);
        assert_eq!(hash.len(), 64); // SHA-256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn test_scan_loader_collection() {
        use std::path::PathBuf;

        let loader_dirs = [
            PathBuf::from(r"D:\Download Folder\edl-flash-master\edl-flash-master\bkerler_edl\Loaders"),
        ];

        let mut total = 0u32;
        let mut with_identity = 0u32;
        let mut with_chipset = 0u32;
        let mut with_pkhash = 0u32;
        let mut sha256_count = 0u32;
        let mut sha384_count = 0u32;
        let mut unknown_chips: Vec<(String, u32)> = Vec::new();

        for dir in &loader_dirs {
            if !dir.exists() {
                eprintln!("Skipping (not found): {}", dir.display());
                continue;
            }
            scan_dir_recursive(dir, &mut total, &mut with_identity, &mut with_chipset,
                &mut with_pkhash, &mut sha256_count, &mut sha384_count, &mut unknown_chips);
        }

        fn scan_dir_recursive(dir: &std::path::Path, total: &mut u32, with_identity: &mut u32,
            with_chipset: &mut u32, with_pkhash: &mut u32, sha256_count: &mut u32,
            sha384_count: &mut u32, unknown_chips: &mut Vec<(String, u32)>) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        scan_dir_recursive(&path, total, with_identity, with_chipset,
                            with_pkhash, sha256_count, sha384_count, unknown_chips);
                        continue;
                    }
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if !matches!(ext, "elf" | "mbn" | "bin") { continue; }

                    *total += 1;
                    if let Some(id) = parse_programmer_identity(&path) {
                        *with_identity += 1;
                        if id.chipset.is_some() {
                            *with_chipset += 1;
                        } else if id.msm_id != 0 {
                            unknown_chips.push((
                                path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                                id.msm_id
                            ));
                        }
                        if !id.pk_hash.is_empty() {
                            *with_pkhash += 1;
                            match id.hash_algorithm {
                                HashAlgorithm::Sha256 => *sha256_count += 1,
                                HashAlgorithm::Sha384 => *sha384_count += 1,
                            }
                        }
                    }
                }
            }
        }

        if total == 0 {
            eprintln!("No loader directories found — skipping verification");
            return;
        }

        // Two metrics:
        // 1. Of all parsed identities (includes anonymous ELF with hw_id=0)
        // 2. Of identities where an MSM_ID was extracted (meaningful denominator)
        let with_msm_id = with_chipset + unknown_chips.len() as u32;
        let chipset_pct_all = if with_identity > 0 {
            (with_chipset as f64 / with_identity as f64) * 100.0
        } else { 0.0 };
        let chipset_pct_msm = if with_msm_id > 0 {
            (with_chipset as f64 / with_msm_id as f64) * 100.0
        } else { 0.0 };
        let anonymous = with_identity.saturating_sub(with_msm_id);

        eprintln!("\n=== Loader Collection Verification ===");
        eprintln!("Total files scanned:   {total}");
        eprintln!("With identity parsed:  {with_identity}");
        eprintln!("  With MSM_ID:         {with_msm_id}");
        eprintln!("  Anonymous (no HWID): {anonymous}");
        eprintln!("With chipset name:     {with_chipset}");
        eprintln!("  of all identities:   {chipset_pct_all:.1}%");
        eprintln!("  of MSM_ID files:     {chipset_pct_msm:.1}%  ← target metric");
        eprintln!("With PKHash:           {with_pkhash}");
        eprintln!("  SHA-256:             {sha256_count}");
        eprintln!("  SHA-384:             {sha384_count}");
        eprintln!("Unknown chipsets:      {}", unknown_chips.len());
        for (name, msm) in &unknown_chips {
            eprintln!("  {name}: 0x{msm:08X}");
        }
        eprintln!("======================================\n");

        // Target: >90% chipset identification among files where MSM_ID was extracted.
        // Anonymous ELF programmers (hw_id=0 in both binary and filename) are excluded
        // from the denominator as they carry no chipset identification data.
        assert!(chipset_pct_msm > 90.0,
            "Chipset identification rate {chipset_pct_msm:.1}% below 90% target (among files with MSM_ID)");
    }
}
