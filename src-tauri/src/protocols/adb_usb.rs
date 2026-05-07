// Protocol infrastructure for ADB direct-USB.
// CNXN/AUTH handshake, message dispatcher, UsbStream transport.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use nusb::transfer::{Direction, EndpointType, RequestBuffer};
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::hazmat::PrehashSigner;
use rsa::signature::SignatureEncoding;
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, RsaPrivateKey};
use sha1::Sha1;
use tracing::{debug, info, warn};

use crate::error::FlashError;

// ---------------------------------------------------------------------------
// ADB wire protocol constants (AOSP adb.h)
// ---------------------------------------------------------------------------

const A_CNXN: u32 = 0x4E584E43; // "CNXN"
const A_AUTH: u32 = 0x48545541; // "AUTH"
const A_OPEN: u32 = 0x4E45504F; // "OPEN"
const A_WRTE: u32 = 0x45545257; // "WRTE"
const A_OKAY: u32 = 0x59414B4F; // "OKAY"
const A_CLSE: u32 = 0x45534C43; // "CLSE"
const A_VERSION: u32 = 0x01000001;
const MAX_PAYLOAD: u32 = 1048576; // 1 MB
const AUTH_TOKEN: u32 = 1;
const AUTH_SIGNATURE: u32 = 2;
const AUTH_RSAPUBLICKEY: u32 = 3;
const ADB_HEADER_SIZE: usize = 24;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct AdbMessage {
    command: u32,
    arg0: u32,
    arg1: u32,
    data: Vec<u8>,
}

/// Information extracted from the ADB CNXN banner.
#[derive(Debug, Clone)]
pub struct BannerInfo {
    /// Device state: "device", "recovery", "sideload", or "unknown".
    pub state: String,
    /// Feature strings, e.g. `["shell_v2", "cmd", "stat_v2"]`.
    #[allow(dead_code)] // Read in tests; has_shell_v2 serves production
    pub features: Vec<String>,
    /// Convenience flag: true when "shell_v2" is present in features.
    pub has_shell_v2: bool,
}

// ---------------------------------------------------------------------------
// Protocol helpers
// ---------------------------------------------------------------------------

/// Sum of all bytes in `data`, wrapping at u32 boundaries.
fn adb_checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32))
}

/// Build a 24-byte ADB message header (WITHOUT payload).
///
/// Always populates the real `data_check` checksum. Modern adbd
/// (Android 9+) negotiates `A_VERSION_SKIP_CHECKSUM` and ignores the field,
/// so a real checksum is harmless. Pre-Android-9 adbd (e.g. Nexus 6P /
/// Android 8.1) validates strictly and silently drops CNXN with a wrong
/// `data_check` — sending the real value keeps both eras compatible.
fn build_header(command: u32, arg0: u32, arg1: u32, payload: &[u8]) -> Vec<u8> {
    let data_len = payload.len() as u32;
    let checksum = adb_checksum(payload);
    let magic = command ^ 0xFFFF_FFFF;

    let mut buf = Vec::with_capacity(ADB_HEADER_SIZE);
    buf.extend_from_slice(&command.to_le_bytes());
    buf.extend_from_slice(&arg0.to_le_bytes());
    buf.extend_from_slice(&arg1.to_le_bytes());
    buf.extend_from_slice(&data_len.to_le_bytes());
    buf.extend_from_slice(&checksum.to_le_bytes());
    buf.extend_from_slice(&magic.to_le_bytes());
    buf
}

/// Parse a raw byte slice into an [`AdbMessage`].
///
/// Validates:
/// - Buffer is at least `ADB_HEADER_SIZE` (24) bytes.
/// - `magic` field equals `command XOR 0xFFFF_FFFF`.
/// - `checksum` field matches the computed checksum of the payload.
#[allow(dead_code)] // Used in tests for message validation
fn parse_message(raw: &[u8]) -> Result<AdbMessage, FlashError> {
    if raw.len() < ADB_HEADER_SIZE {
        return Err(FlashError::Protocol(format!(
            "ADB message too short: {} bytes (need at least {})",
            raw.len(),
            ADB_HEADER_SIZE
        )));
    }

    let command = u32::from_le_bytes(raw[0..4].try_into().expect("ADB header: command field"));
    let arg0 = u32::from_le_bytes(raw[4..8].try_into().expect("ADB header: arg0 field"));
    let arg1 = u32::from_le_bytes(raw[8..12].try_into().expect("ADB header: arg1 field"));
    let data_length = u32::from_le_bytes(raw[12..16].try_into().expect("ADB header: data_length field")) as usize;
    let checksum = u32::from_le_bytes(raw[16..20].try_into().expect("ADB header: checksum field"));
    let magic = u32::from_le_bytes(raw[20..24].try_into().expect("ADB header: magic field"));

    // Validate magic.
    let expected_magic = command ^ 0xFFFF_FFFF;
    if magic != expected_magic {
        return Err(FlashError::Protocol(format!(
            "ADB bad magic: got 0x{:08X}, expected 0x{:08X}",
            magic, expected_magic
        )));
    }

    // Slice payload — allow partial data (use what's available up to data_length).
    let payload = &raw[ADB_HEADER_SIZE..ADB_HEADER_SIZE + data_length.min(raw.len() - ADB_HEADER_SIZE)];

    // Validate checksum — at version 0x01000001, checksum is zero (skip validation).
    if checksum != 0 {
        let computed = adb_checksum(payload);
        if computed != checksum {
            return Err(FlashError::Protocol(format!(
                "ADB checksum mismatch: got 0x{:08X}, expected 0x{:08X}",
                computed, checksum
            )));
        }
    }

    Ok(AdbMessage {
        command,
        arg0,
        arg1,
        data: payload.to_vec(),
    })
}

// ---------------------------------------------------------------------------
// CNXN banner parsing
// ---------------------------------------------------------------------------

/// Parse the banner string from an ADB CNXN packet.
///
/// Format: `{state}::{properties};features={feat1},{feat2},...`
///
/// Examples:
/// - `"device::ro.product.model=SM-G9810;features=shell_v2,cmd"`
/// - `"recovery::features=shell_v2"`
/// - `"sideload::"`
pub fn parse_cnxn_banner(banner: &str) -> BannerInfo {
    // Strip trailing NUL bytes that ADB sometimes appends.
    let banner = banner.trim_end_matches('\0');

    // Extract state — everything before "::".
    let state = banner
        .split("::")
        .next()
        .unwrap_or("")
        .to_string();

    let state = if state.is_empty() {
        "unknown".to_string()
    } else {
        state
    };

    // Find "features=" in the remainder.
    let features: Vec<String> = if let Some(feat_pos) = banner.find("features=") {
        let feat_str = &banner[feat_pos + "features=".len()..];
        // Features end at the next ';' or end of string.
        let feat_str = feat_str.split(';').next().unwrap_or("");
        if feat_str.is_empty() {
            vec![]
        } else {
            feat_str.split(',').map(|s| s.to_string()).collect()
        }
    } else {
        vec![]
    };

    let has_shell_v2 = features.iter().any(|f| f == "shell_v2");

    BannerInfo {
        state,
        features,
        has_shell_v2,
    }
}

// ---------------------------------------------------------------------------
// RSA auth helpers (Task 4)
// ---------------------------------------------------------------------------

/// Return the path to the ADB private key: `~/.android/adbkey`.
fn adb_key_path() -> PathBuf {
    let home = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".android").join("adbkey")
}

/// Load the PKCS#8 PEM private key from `~/.android/adbkey`.
fn load_adb_key() -> Result<RsaPrivateKey, FlashError> {
    let path = adb_key_path();
    let pem = std::fs::read_to_string(&path).map_err(|e| {
        FlashError::Protocol(format!("Failed to read ADB key at {}: {}", path.display(), e))
    })?;
    RsaPrivateKey::from_pkcs8_pem(&pem).map_err(|e| {
        FlashError::Protocol(format!("Failed to parse ADB key: {}", e))
    })
}

/// Sign an AUTH token using PKCS#1 v1.5 with SHA-1 DigestInfo wrapping.
///
/// **CRITICAL**: The 20-byte token IS the pre-computed SHA-1 digest.
/// We use `sign_prehash()` (not `sign()`) to avoid double-hashing.
fn sign_auth_token(key: &RsaPrivateKey, token: &[u8]) -> Result<Vec<u8>, FlashError> {
    let signing_key = SigningKey::<Sha1>::new(key.clone());
    signing_key
        .sign_prehash(token)
        .map(|sig| sig.to_vec())
        .map_err(|e| FlashError::Protocol(format!("RSA token signing failed: {e}")))
}

/// Encode a public key in Android's custom RSAPublicKey format for
/// AUTH RSAPUBLICKEY messages.
///
/// Format: base64(struct) + " {user}@{host}\0"
///
/// The struct is 524 bytes, little-endian:
///   num_words(u32) | n0inv(u32) | n[64](u32) | rr[64](u32) | exponent(u32)
fn encode_android_public_key(key: &RsaPrivateKey) -> Vec<u8> {
    let pub_key = key.to_public_key();
    let n = pub_key.n();
    let e = pub_key.e();

    // Convert modulus to 64 little-endian u32 words.
    let n_bytes = n.to_bytes_le();
    let mut n_words = [0u32; 64];
    bytes_to_le_words(&n_bytes, &mut n_words);

    // n0inv: value such that n0 * n0inv ≡ -1 (mod 2^32)
    // We compute: n0inv = (2^32 - (n^(-1) mod 2^32)) mod 2^32
    // which satisfies n0 * n0inv ≡ -1 (mod 2^32).
    let n0 = n_words[0] as u64;
    // Extended Euclidean to find modular inverse of n0 mod 2^32,
    // then negate. Since n is odd (RSA modulus), n0 is odd, inverse exists.
    let modulus = 1u64 << 32;
    let n0_inv_raw = mod_inverse_u32(n0, modulus);
    let n0inv = ((modulus - n0_inv_raw) % modulus) as u32;

    // rr = (2^4096) mod n
    let two_pow_4096 = BigUint::from(1u32) << 4096;
    let rr = two_pow_4096 % n;
    let rr_bytes = rr.to_bytes_le();
    let mut rr_words = [0u32; 64];
    bytes_to_le_words(&rr_bytes, &mut rr_words);

    // Extract public exponent as u32. Standard RSA uses 65537 (0x10001).
    let e_bytes = e.to_bytes_le();
    let mut e_buf = [0u8; 4];
    let copy_len = e_bytes.len().min(4);
    e_buf[..copy_len].copy_from_slice(&e_bytes[..copy_len]);
    let exp_u32 = u32::from_le_bytes(e_buf);

    // Build the 524-byte struct.
    let mut buf = Vec::with_capacity(524);
    buf.extend_from_slice(&64u32.to_le_bytes()); // num_words
    buf.extend_from_slice(&n0inv.to_le_bytes());
    for &w in &n_words {
        buf.extend_from_slice(&w.to_le_bytes());
    }
    for &w in &rr_words {
        buf.extend_from_slice(&w.to_le_bytes());
    }
    buf.extend_from_slice(&exp_u32.to_le_bytes());
    debug_assert_eq!(buf.len(), 524);

    // Base64-encode the struct.
    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);

    // Build user@host identifier.
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string());

    // Final format: base64 + space + user@host + NUL
    let result = format!("{} {}@{}\0", b64, user, host).into_bytes();
    // Ensure null terminator is present (format! already added it as \0 char).
    debug_assert_eq!(*result.last().unwrap(), 0u8);
    result
}

/// Fill a `[u32; N]` array from a little-endian byte slice, zero-padding as needed.
fn bytes_to_le_words(bytes: &[u8], words: &mut [u32]) {
    for (i, word) in words.iter_mut().enumerate() {
        let offset = i * 4;
        if offset + 4 <= bytes.len() {
            *word = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4-byte aligned RSA word"));
        } else if offset < bytes.len() {
            let mut buf = [0u8; 4];
            buf[..bytes.len() - offset].copy_from_slice(&bytes[offset..]);
            *word = u32::from_le_bytes(buf);
        }
    }
}

/// Compute modular inverse of `a` mod `m` using extended Euclidean algorithm.
/// Requires that gcd(a, m) == 1 (always true for odd RSA n0 mod 2^32).
fn mod_inverse_u32(a: u64, m: u64) -> u64 {
    let (mut old_r, mut r) = (a as i128, m as i128);
    let (mut old_s, mut s) = (1i128, 0i128);

    while r != 0 {
        let q = old_r / r;
        let tmp = r;
        r = old_r - q * r;
        old_r = tmp;
        let tmp = s;
        s = old_s - q * s;
        old_s = tmp;
    }

    ((old_s % m as i128 + m as i128) % m as i128) as u64
}

// ---------------------------------------------------------------------------
// Task 5: USB Bulk I/O Helpers
// ---------------------------------------------------------------------------

/// Check if a USB transfer error indicates device disconnection.
fn is_disconnect_error(err: &nusb::transfer::TransferError) -> bool {
    matches!(err, nusb::transfer::TransferError::Disconnected)
        || err.to_string().to_lowercase().contains("disconnect")
}

/// Maximum read buffer size for a single bulk IN transfer.
const USB_READ_BUF: usize = 16384;

/// Send an ADB message as TWO separate USB bulk OUT transfers (header then payload).
/// The ADB USB protocol requires header and payload as separate transfers —
/// the device reads exactly 24 bytes for the header, then issues a second read
/// for the payload. Combining them into one transfer causes STALL.
fn usb_send_message(
    interface: &nusb::Interface,
    ep_out: u8,
    command: u32,
    arg0: u32,
    arg1: u32,
    data: &[u8],
) -> Result<(), FlashError> {
    // Transfer 1: 24-byte header
    let header = build_header(command, arg0, arg1, data);
    pollster::block_on(interface.bulk_out(ep_out, header))
        .into_result()
        .map_err(|e| {
            if is_disconnect_error(&e) {
                FlashError::DeviceDisconnected
            } else {
                FlashError::Usb(format!("bulk OUT (header) failed: {e}"))
            }
        })?;

    // Transfer 2: payload (if non-empty)
    if !data.is_empty() {
        pollster::block_on(interface.bulk_out(ep_out, data.to_vec()))
            .into_result()
            .map_err(|e| {
                if is_disconnect_error(&e) {
                    FlashError::DeviceDisconnected
                } else {
                    FlashError::Usb(format!("bulk OUT (payload) failed: {e}"))
                }
            })?;
    }
    Ok(())
}

/// Perform a single bulk_in transfer with a real per-transfer deadline.
///
/// When called from inside a tokio runtime (e.g. spawn_blocking from a Tauri
/// command), uses `tokio::time::timeout` to truly cancel the in-flight USB
/// transfer if the deadline expires — the dropped future signals nusb to
/// cancel the kernel-level URB. When called outside a tokio runtime (e.g.
/// the dispatcher's `std::thread`), falls back to `pollster::block_on` and
/// relies on the caller's between-read deadline checks.
fn cancellable_bulk_in(
    interface: &nusb::Interface,
    ep: u8,
    capacity: usize,
    timeout: Duration,
    where_: &'static str,
) -> Result<Vec<u8>, FlashError> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let buf = RequestBuffer::new(capacity);
        let interface = interface.clone();
        return handle.block_on(async move {
            match tokio::time::timeout(timeout, interface.bulk_in(ep, buf)).await {
                Ok(completion) => completion.into_result().map_err(|e| {
                    if is_disconnect_error(&e) {
                        FlashError::DeviceDisconnected
                    } else {
                        FlashError::Usb(format!("bulk IN ({where_}) failed: {e}"))
                    }
                }),
                Err(_) => Err(FlashError::Protocol(format!(
                    "USB bulk IN ({where_}) timeout after {} ms — device sent no data",
                    timeout.as_millis()
                ))),
            }
        });
    }
    let buf = RequestBuffer::new(capacity);
    let completion = pollster::block_on(interface.bulk_in(ep, buf));
    completion.into_result().map_err(|e| {
        if is_disconnect_error(&e) {
            FlashError::DeviceDisconnected
        } else {
            FlashError::Usb(format!("bulk IN ({where_}) failed: {e}"))
        }
    })
}

/// Read one complete ADB message from USB bulk IN.
///
/// Reads the 24-byte header first, validates magic, then reads the payload
/// if `data_length > 0`.
fn usb_read_message(
    interface: &nusb::Interface,
    ep_in: u8,
    timeout: Duration,
) -> Result<AdbMessage, FlashError> {
    let deadline = Instant::now() + timeout;

    // --- Read header (24 bytes) ---
    let mut header_buf = Vec::new();
    while header_buf.len() < ADB_HEADER_SIZE {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FlashError::Protocol(format!(
                "USB read timeout waiting for ADB header (got {} of {} bytes)",
                header_buf.len(),
                ADB_HEADER_SIZE
            )));
        }

        let need = ADB_HEADER_SIZE - header_buf.len();
        let data = cancellable_bulk_in(interface, ep_in, need.max(64), remaining, "header")?;
        header_buf.extend_from_slice(&data);
    }

    // Parse header fields.
    let command = u32::from_le_bytes(header_buf[0..4].try_into().expect("USB header: command field"));
    let arg0 = u32::from_le_bytes(header_buf[4..8].try_into().expect("USB header: arg0 field"));
    let arg1 = u32::from_le_bytes(header_buf[8..12].try_into().expect("USB header: arg1 field"));
    let data_length = u32::from_le_bytes(header_buf[12..16].try_into().expect("USB header: data_length field")) as usize;
    let checksum = u32::from_le_bytes(header_buf[16..20].try_into().expect("USB header: checksum field"));
    let magic = u32::from_le_bytes(header_buf[20..24].try_into().expect("USB header: magic field"));

    // Validate magic.
    let expected_magic = command ^ 0xFFFF_FFFF;
    if magic != expected_magic {
        return Err(FlashError::Protocol(format!(
            "ADB bad magic: got 0x{magic:08X}, expected 0x{expected_magic:08X}"
        )));
    }

    // Bounds-check payload size before allocating.
    if data_length > MAX_PAYLOAD as usize {
        return Err(FlashError::Protocol(format!(
            "ADB message payload too large: {} bytes (max {})", data_length, MAX_PAYLOAD
        )));
    }

    // --- Read payload ---
    // If the header read returned more than 24 bytes (common for small
    // messages where header + payload arrive in one USB transfer), the
    // excess bytes are the start of the payload — prepend them.
    let excess = header_buf[ADB_HEADER_SIZE..].to_vec();
    let mut payload = excess;
    while payload.len() < data_length {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FlashError::Protocol(format!(
                "USB read timeout waiting for ADB payload (got {} of {} bytes)",
                payload.len(),
                data_length
            )));
        }

        let need = data_length - payload.len();
        let data = cancellable_bulk_in(interface, ep_in, need.min(USB_READ_BUF), remaining, "payload")?;
        payload.extend_from_slice(&data);
    }

    // Validate checksum — at version 0x01000001, checksum is zero (skip validation).
    if checksum != 0 {
        let computed = adb_checksum(&payload);
        if computed != checksum {
            return Err(FlashError::Protocol(format!(
                "ADB checksum mismatch: got 0x{computed:08X}, expected 0x{checksum:08X}"
            )));
        }
    }

    Ok(AdbMessage {
        command,
        arg0,
        arg1,
        data: payload,
    })
}

// ---------------------------------------------------------------------------
// Task 6: USB Connection — CNXN + AUTH Handshake
// ---------------------------------------------------------------------------

/// ADB interface class identifiers (per AOSP USB spec).
const ADB_CLASS: u8 = 0xFF;
const ADB_SUBCLASS: u8 = 0x42;
const ADB_PROTOCOL: u8 = 0x01;

/// An established ADB-over-USB connection (post-CNXN handshake).
pub struct UsbConnection {
    /// Shared interface for writes (through dispatcher).
    pub write_interface: Arc<Mutex<nusb::Interface>>,
    /// Bulk OUT endpoint address.
    pub ep_out: u8,
    /// Maximum payload size negotiated with the device.
    pub max_payload: u32,
    /// Parsed CNXN banner info from the device.
    pub banner: BannerInfo,
    /// Device serial number.
    #[allow(dead_code)] // Stored as connection context for debugging
    pub serial: String,
    /// Dispatcher for registering streams.
    pub(crate) dispatcher: Arc<UsbDispatcher>,
}

// ---------------------------------------------------------------------------
// Authenticated USB connection cache
// ---------------------------------------------------------------------------

/// Cached authenticated USB connection (post-CNXN handshake).
/// Owns the message dispatcher that reads from the USB endpoint.
struct CachedUsbConnection {
    ep_out: u8,
    max_payload: u32,
    banner: BannerInfo,
    /// Message dispatcher — single reader thread routing to streams.
    dispatcher: Arc<UsbDispatcher>,
}

fn usb_conn_cache() -> &'static Mutex<HashMap<String, CachedUsbConnection>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<String, CachedUsbConnection>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// USB Message Dispatcher — single reader thread with channel routing
// ---------------------------------------------------------------------------

/// Single-reader message dispatcher for ADB-over-USB.
///
/// AOSP architecture: one reader thread per USB connection reads ALL messages
/// from bulk_in and routes them by local_id to per-stream channels. No stream
/// ever reads from USB directly. This eliminates the concurrent-reader race
/// that causes STALL and bad-magic errors.
pub(crate) struct UsbDispatcher {
    /// Registry of active streams: local_id -> message sender.
    /// The reader thread holds a clone of this Arc.
    streams: Arc<Mutex<HashMap<u32, mpsc::Sender<AdbMessage>>>>,
    /// Shared interface for writes (serialized by Mutex).
    write_interface: Arc<Mutex<nusb::Interface>>,
    /// Bulk OUT endpoint address.
    #[allow(dead_code)] // Stored for future dispatcher-level writes
    ep_out: u8,
    /// Shutdown signal -- set to true to stop the reader thread.
    shutdown: Arc<AtomicBool>,
    /// Reader thread handle -- joined on drop.
    reader_handle: Option<std::thread::JoinHandle<()>>,
}

impl UsbDispatcher {
    /// Start the dispatcher for a USB connection.
    ///
    /// Spawns a background thread that reads ADB messages from bulk_in
    /// and routes them to registered streams by local_id.
    fn start(interface: nusb::Interface, ep_in: u8, ep_out: u8, serial: String) -> Result<Self, FlashError> {
        let streams: Arc<Mutex<HashMap<u32, mpsc::Sender<AdbMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let write_interface = Arc::new(Mutex::new(interface.clone()));

        let reader_streams = streams.clone();
        let reader_shutdown = shutdown.clone();

        let reader_handle = std::thread::Builder::new()
            .name(format!("usb-dispatch-{serial}"))
            .spawn(move || {
                Self::reader_loop(interface, ep_in, reader_streams, reader_shutdown, serial);
            })
            .map_err(|e| FlashError::Usb(format!("Failed to spawn USB dispatcher thread: {e}")))?;

        Ok(Self {
            streams,
            write_interface,
            ep_out,
            shutdown,
            reader_handle: Some(reader_handle),
        })
    }

    /// The reader loop -- runs on a dedicated OS thread.
    /// Reads one ADB message at a time and routes to the correct stream's channel.
    fn reader_loop(
        interface: nusb::Interface,
        ep_in: u8,
        streams: Arc<Mutex<HashMap<u32, mpsc::Sender<AdbMessage>>>>,
        shutdown: Arc<AtomicBool>,
        serial: String,
    ) {
        debug!(serial, "USB dispatcher reader started");

        loop {
            if shutdown.load(Ordering::Relaxed) {
                debug!(serial, "USB dispatcher shutting down (signal)");
                break;
            }

            // Read with 5s timeout so we periodically check the shutdown flag.
            match usb_read_message(&interface, ep_in, Duration::from_secs(5)) {
                Ok(msg) => {
                    let target_id = msg.arg1;
                    let cmd = msg.command;
                    if let Ok(map) = streams.lock() {
                        if let Some(tx) = map.get(&target_id) {
                            if tx.send(msg).is_err() {
                                debug!(serial, target_id, "Stream receiver dropped, discarding message");
                            }
                        } else {
                            // No stream registered for this local_id -- discard.
                            // This is normal for stale messages from previous streams.
                            debug!(serial, target_id, cmd = format_args!("0x{cmd:08X}"),
                                "Dispatcher: no stream for local_id, discarding");
                        }
                    }
                }
                Err(FlashError::Protocol(ref msg)) if msg.contains("timeout") => {
                    // Normal -- no data available, loop and check shutdown.
                    continue;
                }
                Err(FlashError::Protocol(ref msg)) if msg.contains("bad magic") => {
                    // Stale data from previous session -- skip it.
                    debug!(serial, "Dispatcher: skipping stale data (bad magic)");
                    continue;
                }
                Err(FlashError::DeviceDisconnected) => {
                    info!(serial, "USB dispatcher: device disconnected");
                    shutdown.store(true, Ordering::Relaxed);
                    break;
                }
                Err(e) => {
                    warn!(serial, error = %e, "USB dispatcher read error, stopping");
                    shutdown.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }

        // Drop all stream senders so recv_timeout returns Disconnected immediately
        // instead of blocking for the full read timeout (was 10s delay).
        if let Ok(mut map) = streams.lock() {
            map.clear();
        }
        debug!(serial, "USB dispatcher reader stopped");
    }

    /// Register a stream to receive messages for the given local_id.
    fn register_stream(&self, local_id: u32) -> Result<mpsc::Receiver<AdbMessage>, FlashError> {
        let (tx, rx) = mpsc::channel();
        let mut map = self.streams.lock()
            .map_err(|_| FlashError::Protocol("Dispatcher streams lock poisoned".into()))?;
        map.insert(local_id, tx);
        Ok(rx)
    }

    /// Unregister a stream (called on UsbStream drop).
    fn unregister_stream(&self, local_id: u32) {
        if let Ok(mut map) = self.streams.lock() {
            map.remove(&local_id);
        }
    }

    /// Signal the reader thread to stop.
    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Check if the dispatcher is still running.
    fn is_alive(&self) -> bool {
        !self.shutdown.load(Ordering::Relaxed)
    }
}

impl Drop for UsbDispatcher {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(handle) = self.reader_handle.take() {
            // Give the reader time to finish (it has a 5s read timeout).
            let _ = handle.join();
        }
    }
}

/// Invalidate the cached USB connection for a serial.
/// Called on disconnect or when the connection is known to be stale.
pub fn invalidate_usb_cache(serial: &str) {
    if let Ok(mut cache) = usb_conn_cache().lock() {
        if let Some(cached) = cache.remove(serial) {
            cached.dispatcher.shutdown();
            info!(serial, "Invalidated cached USB connection (dispatcher stopped)");
        }
    }
}

/// Clear all cached USB connections, shutting down their dispatchers.
pub fn clear_usb_cache() {
    if let Ok(mut cache) = usb_conn_cache().lock() {
        for (_, cached) in cache.drain() {
            cached.dispatcher.shutdown();
        }
        info!("Cleared all cached USB connections");
    }
}

/// Scan USB bus for an ADB-capable device with the given serial number.
///
/// Returns (device, interface_number, ep_in, ep_out).
///
/// On Windows, the `DeviceInfo::interfaces()` may be empty for non-composite
/// devices. In that case we fall back to opening the device and scanning the
/// active configuration descriptor for the ADB interface.
pub fn find_adb_usb_device(
    target_serial: &str,
) -> Result<(nusb::Device, u8, u8, u8), FlashError> {
    let devices = nusb::list_devices().map_err(|e| FlashError::Usb(e.to_string()))?;

    for dev_info in devices {
        // Match by serial number.
        let serial = match dev_info.serial_number() {
            Some(s) => s,
            None => continue,
        };
        if serial != target_serial {
            continue;
        }

        // Open device and scan configuration descriptor for ADB interface.
        let device = dev_info
            .open()
            .map_err(|e| FlashError::Usb(format!("Failed to open device: {e}")))?;

        if let Some((intf_num, ep_in, ep_out)) = find_adb_from_config(&device) {
            info!(
                serial = target_serial,
                intf = intf_num,
                ep_in = format_args!("0x{ep_in:02X}"),
                ep_out = format_args!("0x{ep_out:02X}"),
                "Found ADB device via config descriptor"
            );
            return Ok((device, intf_num, ep_in, ep_out));
        }

        // Device matched serial but no ADB interface found.
        return Err(FlashError::Usb(format!(
            "Device '{target_serial}' found but has no ADB interface (class=0xFF/0x42/0x01)"
        )));
    }

    Err(FlashError::NoDevice)
}

/// Scan the device's active configuration descriptor for the ADB interface
/// and its bulk endpoints.
pub(crate) fn find_adb_from_config(device: &nusb::Device) -> Option<(u8, u8, u8)> {
    let config = device.active_configuration().ok()?;

    for alt_setting in config.interface_alt_settings() {
        if alt_setting.class() == ADB_CLASS
            && alt_setting.subclass() == ADB_SUBCLASS
            && alt_setting.protocol() == ADB_PROTOCOL
        {
            let mut ep_in = None;
            let mut ep_out = None;

            for ep in alt_setting.endpoints() {
                if ep.transfer_type() == EndpointType::Bulk {
                    match ep.direction() {
                        Direction::In => ep_in = Some(ep.address()),
                        Direction::Out => ep_out = Some(ep.address()),
                    }
                }
            }

            if let (Some(ei), Some(eo)) = (ep_in, ep_out) {
                info!(
                    intf = alt_setting.interface_number(),
                    ep_in = format_args!("0x{ei:02X}"),
                    ep_out = format_args!("0x{eo:02X}"),
                    "Found ADB interface"
                );
                return Some((alt_setting.interface_number(), ei, eo));
            }
        }
    }

    None
}

/// Kill the ADB server to release USB interface.
/// Sends `host:kill` to localhost:5037, then verifies the server actually died
/// by polling the port. Waits up to ~2s total. No-op if server isn't running.
pub fn kill_adb_server() {
    use std::io::Write;
    use std::net::TcpStream;

    let addr: std::net::SocketAddr = "127.0.0.1:5037".parse().expect("hardcoded address");

    // Try to connect — if server is not running, no-op.
    let stream = TcpStream::connect_timeout(&addr, Duration::from_millis(200));
    if let Ok(mut stream) = stream {
        let cmd = "host:kill";
        let msg = format!("{:04x}{cmd}", cmd.len());
        let _ = stream.write_all(msg.as_bytes());
        let _ = stream.flush();
        drop(stream);

        // Verify server actually died by checking if the port is released.
        // This is more reliable than a fixed sleep — Windows process teardown
        // and USB interface release can take variable time.
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_err() {
                info!("ADB server killed and port released");
                // Extra grace for USB interface release after process exit.
                std::thread::sleep(Duration::from_millis(200));
                return;
            }
        }
        // Server didn't die cleanly in 2s — proceed anyway.
        warn!("ADB server kill sent but port still open after 2s");
    }
}

/// Perform the full ADB USB connection handshake (CNXN + AUTH).
pub fn adb_usb_connect(serial: &str) -> Result<UsbConnection, FlashError> {
    // Hold the cache mutex for the entire connection establishment.
    // This serializes concurrent callers — second thread waits while first
    // completes claim + CNXN + AUTH, then reuses the cached connection.
    let mut cache = usb_conn_cache().lock()
        .map_err(|_| FlashError::Protocol("USB connection cache lock poisoned".into()))?;

    // Check cache — reuse existing authenticated connection.
    if let Some(cached) = cache.get(serial) {
        if cached.dispatcher.is_alive() {
            info!(serial, "Reusing cached USB connection");
            return Ok(UsbConnection {
                write_interface: cached.dispatcher.write_interface.clone(),
                ep_out: cached.ep_out,
                max_payload: cached.max_payload,
                banner: cached.banner.clone(),
                serial: serial.to_string(),
                dispatcher: cached.dispatcher.clone(),
            });
        }
        // Dispatcher died — remove stale cache entry and reconnect.
        info!(serial, "Cached dispatcher is dead, reconnecting");
        cache.remove(serial);
    }

    // No cached connection — do full claim + CNXN + AUTH handshake.
    // Cache mutex is held, so concurrent callers block here.

    // Kill ADB server first to release USB interface — it may be holding
    // exclusive access even if the user thinks it's not running.
    kill_adb_server();

    let (device, intf_num, ep_in, ep_out) = find_adb_usb_device(serial)?;

    let interface = device
        .claim_interface(intf_num)
        .map_err(|e| FlashError::Usb(format!("Failed to claim interface {intf_num}: {e}")))?;

    // Note: We intentionally do NOT call set_alt_setting(0) here.
    // ADB interface only has one alternate setting (0), and calling
    // SET_INTERFACE on Windows composite devices causes USBCCGP driver
    // to refresh the device tree, making Device Manager "shake".
    // See: https://github.com/kevinmehall/nusb/issues/...

    // Clear any stale STALL / halt condition on both bulk endpoints.
    // A previous session that was interrupted mid-transfer can leave an
    // endpoint halted; nusb does NOT clear this automatically on claim.
    //
    // Linux note: on usbfs, clear_halt translates to USBDEVFS_CLR_HALT,
    // which on some adbd builds (e.g. Nexus 6P / Android 8.1) causes the
    // device-side ADB function to tear down its USB endpoints and
    // renumerate, breaking the very session we're about to open. WinUSB on
    // Windows doesn't have this side effect, so keep the safety net there.
    #[cfg(not(target_os = "linux"))]
    {
        interface.clear_halt(ep_in).map_err(|e| {
            FlashError::Usb(format!("Failed to clear halt on EP IN 0x{ep_in:02X}: {e}"))
        })?;
        interface.clear_halt(ep_out).map_err(|e| {
            FlashError::Usb(format!("Failed to clear halt on EP OUT 0x{ep_out:02X}: {e}"))
        })?;
    }

    info!(serial, "USB interface claimed");

    // Send CNXN — header and payload as separate USB bulk transfers.
    // MAX_PAYLOAD (1MB) is correct for arg1 per AOSP (not 4096 — that's for banner size limit).
    let host_banner = b"host::features=shell_v2,cmd";
    info!(serial, "Sending ADB CNXN");
    if let Err(e) = usb_send_message(&interface, ep_out, A_CNXN, A_VERSION, MAX_PAYLOAD, host_banner) {
        cache.remove(serial);
        return Err(e);
    }

    // Read CNXN response, skipping stale data from previous sessions.
    //
    // After claim + clear_halt, the USB IN endpoint may still contain:
    //   a) Raw text (shell output) → usb_read_message returns "bad magic"
    //   b) Valid ADB messages (WRTE/CLSE/OKAY) from old streams → valid parse, wrong command
    // Both cases are handled by retrying up to 32 times.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stale_skipped = 0u32;
    let (max_payload, banner) = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || stale_skipped > 32 {
            cache.remove(serial);
            return Err(FlashError::Protocol(
                "Too many stale messages in USB endpoint during CNXN handshake".into(),
            ));
        }

        let msg = match usb_read_message(&interface, ep_in, remaining) {
            Ok(m) => m,
            Err(FlashError::Protocol(ref msg_text)) if msg_text.contains("bad magic") => {
                stale_skipped += 1;
                debug!(serial, stale_skipped, "Stale data in USB endpoint (bad magic), retrying");
                continue;
            }
            Err(e) => {
                cache.remove(serial);
                return Err(e);
            }
        };

        match msg.command {
            A_CNXN => {
                if stale_skipped > 0 {
                    debug!(serial, stale_skipped, "Skipped stale messages before CNXN response");
                }
                info!(serial, "ADB CNXN accepted (no auth required)");
                let max_payload = msg.arg1.min(MAX_PAYLOAD);
                let banner_str = String::from_utf8_lossy(&msg.data);
                let banner = parse_cnxn_banner(&banner_str);
                break (max_payload, banner);
            }
            A_AUTH if msg.arg0 == AUTH_TOKEN => {
                if stale_skipped > 0 {
                    debug!(serial, stale_skipped, "Skipped stale messages before AUTH TOKEN");
                }
                info!(serial, "ADB AUTH TOKEN received, signing...");
                match do_auth_handshake(&interface, ep_in, ep_out, &msg.data) {
                    Ok(result) => break result,
                    Err(e) => {
                        cache.remove(serial);
                        return Err(e);
                    }
                }
            }
            A_WRTE | A_OKAY | A_CLSE | A_AUTH => {
                // Stale ADB message from a previous session — discard and retry.
                stale_skipped += 1;
                debug!(serial, stale_skipped, cmd = format_args!("0x{:08X}", msg.command),
                    "Discarding stale ADB message during CNXN handshake");
                continue;
            }
            other => {
                cache.remove(serial);
                return Err(FlashError::Protocol(format!(
                    "Unexpected response to CNXN: command=0x{other:08X}"
                )));
            }
        }
    };

    // Start message dispatcher — single reader thread for this connection.
    let dispatcher = Arc::new(UsbDispatcher::start(
        interface, ep_in, ep_out, serial.to_string()
    )?);

    // Cache the authenticated connection for reuse (mutex already held).
    cache.insert(serial.to_string(), CachedUsbConnection {
        ep_out,
        max_payload,
        banner: banner.clone(),
        dispatcher: dispatcher.clone(),
    });

    Ok(UsbConnection {
        write_interface: dispatcher.write_interface.clone(),
        ep_out,
        max_payload,
        banner,
        serial: serial.to_string(),
        dispatcher,
    })
}

/// Handle the AUTH challenge/response flow.
///
/// 1. Sign the AUTH TOKEN with our private key → send AUTH SIGNATURE.
/// 2. If device sends another AUTH TOKEN → send AUTH RSAPUBLICKEY and wait
///    for user to accept on device (30 s timeout).
fn do_auth_handshake(
    interface: &nusb::Interface,
    ep_in: u8,
    ep_out: u8,
    token: &[u8],
) -> Result<(u32, BannerInfo), FlashError> {
    let key = load_adb_key()?;

    // Step 1: Sign token and send AUTH SIGNATURE.
    let signature = sign_auth_token(&key, token)?;
    usb_send_message(interface, ep_out, A_AUTH, AUTH_SIGNATURE, 0, &signature)?;

    let resp = usb_read_message(interface, ep_in, Duration::from_secs(10))?;

    match resp.command {
        A_CNXN => {
            // Device accepted the signature (key was already trusted).
            info!("ADB AUTH SIGNATURE accepted");
            let max_payload = resp.arg1.min(MAX_PAYLOAD);
            let banner_str = String::from_utf8_lossy(&resp.data);
            let banner = parse_cnxn_banner(&banner_str);
            return Ok((max_payload, banner));
        }
        A_AUTH if resp.arg0 == AUTH_TOKEN => {
            // Device didn't recognize key; send public key for user approval.
            debug!("AUTH SIGNATURE rejected, sending RSAPUBLICKEY for user approval");
        }
        other => {
            return Err(FlashError::Protocol(format!(
                "Unexpected AUTH response: command=0x{other:08X}, arg0={}",
                resp.arg0
            )));
        }
    }

    // Step 2: Send AUTH RSAPUBLICKEY and wait for user to tap "Allow" on device.
    let pubkey_data = encode_android_public_key(&key);
    usb_send_message(interface, ep_out, A_AUTH, AUTH_RSAPUBLICKEY, 0, &pubkey_data)?;
    info!("Sent AUTH RSAPUBLICKEY — waiting for user to accept on device (30 s)...");

    let resp = usb_read_message(interface, ep_in, Duration::from_secs(30))?;

    match resp.command {
        A_CNXN => {
            info!("ADB AUTH accepted by user");
            let max_payload = resp.arg1.min(MAX_PAYLOAD);
            let banner_str = String::from_utf8_lossy(&resp.data);
            let banner = parse_cnxn_banner(&banner_str);
            Ok((max_payload, banner))
        }
        other => Err(FlashError::Protocol(format!(
            "Expected CNXN after RSAPUBLICKEY, got command=0x{other:08X}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Task 7: UsbStream — OPEN + Read/Write
// ---------------------------------------------------------------------------

/// Monotonically increasing local stream ID generator.
static NEXT_LOCAL_ID: AtomicU32 = AtomicU32::new(1);

/// A bidirectional stream over an ADB USB transport, corresponding to a
/// single ADB service (e.g. `shell:`, `sync:`, etc.).
///
/// Implements `std::io::Read` and `std::io::Write` so it can be used
/// anywhere a blocking I/O stream is expected.
pub struct UsbStream {
    /// Shared interface for writes (serialized by Mutex).
    write_interface: Arc<Mutex<nusb::Interface>>,
    ep_out: u8,
    local_id: u32,
    remote_id: u32,
    max_payload: u32,
    read_buf: Vec<u8>,
    /// Position in `read_buf` for the next byte to hand out.
    read_pos: usize,
    closed: bool,
    read_timeout: Option<Duration>,
    /// Channel receiver -- messages routed here by the dispatcher.
    message_rx: mpsc::Receiver<AdbMessage>,
    /// Dispatcher reference for unregistration on drop.
    dispatcher: Arc<UsbDispatcher>,
}

/// Open an ADB service stream over USB.
///
/// Registers with the dispatcher, sends OPEN, then waits for OKAY/CLSE
/// from the channel (routed by the dispatcher's reader thread).
pub fn adb_usb_open(conn: UsbConnection, service: &str) -> Result<UsbStream, FlashError> {
    let local_id = NEXT_LOCAL_ID.fetch_add(1, Ordering::Relaxed);
    let service_bytes = format!("{service}\0");

    // Register with dispatcher BEFORE sending OPEN — so the OKAY response
    // is routed to our channel even if it arrives before we start reading.
    let message_rx = conn.dispatcher.register_stream(local_id)?;

    info!(local_id, service, "Opening ADB stream");

    // Send OPEN through the write interface.
    {
        let iface = conn.write_interface.lock()
            .map_err(|_| FlashError::Protocol("Write lock poisoned".into()))?;
        usb_send_message(&iface, conn.ep_out, A_OPEN, local_id, 0, service_bytes.as_bytes())?;
    }

    // Wait for OKAY/CLSE from our channel (routed by dispatcher).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            conn.dispatcher.unregister_stream(local_id);
            return Err(FlashError::Protocol("Timeout waiting for OKAY after OPEN".into()));
        }

        let msg = match message_rx.recv_timeout(remaining) {
            Ok(msg) => msg,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                conn.dispatcher.unregister_stream(local_id);
                return Err(FlashError::Protocol("Timeout waiting for OKAY after OPEN".into()));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                conn.dispatcher.unregister_stream(local_id);
                return Err(FlashError::Protocol("USB dispatcher stopped during OPEN".into()));
            }
        };

        match msg.command {
            A_OKAY => {
                let remote_id = msg.arg0;
                debug!(local_id, remote_id, "ADB stream opened via dispatcher");
                return Ok(UsbStream {
                    write_interface: conn.write_interface,
                    ep_out: conn.ep_out,
                    local_id,
                    remote_id,
                    max_payload: conn.max_payload,
                    read_buf: Vec::new(),
                    read_pos: 0,
                    closed: false,
                    read_timeout: Some(Duration::from_secs(10)),
                    message_rx,
                    dispatcher: conn.dispatcher,
                });
            }
            A_CLSE => {
                conn.dispatcher.unregister_stream(local_id);
                return Err(FlashError::Protocol(format!(
                    "Device refused to open service '{service}'"
                )));
            }
            _ => {
                // Unexpected message during OPEN — skip (dispatcher may route
                // stale messages from previous streams that had our local_id).
                debug!(local_id, cmd = format_args!("0x{:08X}", msg.command),
                    "Skipping unexpected message during OPEN");
                continue;
            }
        }
    }
}

impl UsbStream {
    /// Set the read timeout. `None` means block indefinitely (not recommended).
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    /// Create a write-only handle for interactive shell.
    /// The writer shares the USB interface but doesn't touch the read channel.
    pub(crate) fn create_shell_writer(&self) -> UsbShellWriter {
        UsbShellWriter {
            write_interface: self.write_interface.clone(),
            ep_out: self.ep_out,
            local_id: self.local_id,
            remote_id: self.remote_id,
            max_payload: self.max_payload,
            closed: false,
            dispatcher: self.dispatcher.clone(),
        }
    }

    /// Try to close the stream gracefully. Ignores errors (best-effort).
    fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            // Send CLSE through write interface (best-effort).
            if let Ok(iface) = self.write_interface.lock() {
                let _ = usb_send_message(&iface, self.ep_out, A_CLSE, self.local_id, self.remote_id, &[]);
            }
        }
    }
}

/// Lightweight write-only handle for interactive shell over USB.
///
/// Sends WRTE messages without waiting for OKAY — safe for shell keystrokes
/// (tiny payloads, no backpressure needed). The reader thread handles OKAY acks.
/// NOT suitable for large data transfers — lacks flow control for payloads near max_payload.
pub struct UsbShellWriter {
    write_interface: Arc<Mutex<nusb::Interface>>,
    ep_out: u8,
    local_id: u32,
    remote_id: u32,
    max_payload: u32,
    closed: bool,
    dispatcher: Arc<UsbDispatcher>,
}

impl UsbShellWriter {
    pub fn shutdown(&mut self) {
        // Don't send CLSE — the reader (UsbStream) owns the stream lifecycle
        // and sends CLSE in its Drop. Sending two CLSE is harmless but unnecessary.
        self.closed = true;
    }
}

impl io::Write for UsbShellWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.closed || !self.dispatcher.is_alive() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "USB shell stream is closed"));
        }
        let chunk_size = buf.len().min(self.max_payload as usize);
        let chunk = &buf[..chunk_size];
        let iface = self.write_interface.lock()
            .map_err(|_| io::Error::other("write lock poisoned"))?;
        usb_send_message(&iface, self.ep_out, A_WRTE, self.local_id, self.remote_id, chunk)
            .map_err(flash_error_to_io)?;
        Ok(chunk_size)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for UsbShellWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Convert FlashError to io::Error, preserving disconnect semantics.
fn flash_error_to_io(e: FlashError) -> io::Error {
    match e {
        FlashError::DeviceDisconnected => {
            io::Error::new(io::ErrorKind::ConnectionReset, e.to_string())
        }
        _ => io::Error::other(e.to_string()),
    }
}

impl io::Read for UsbStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.closed {
            return Ok(0);
        }

        // Drain buffered data first.
        if self.read_pos < self.read_buf.len() {
            let available = &self.read_buf[self.read_pos..];
            let n = available.len().min(buf.len());
            buf[..n].copy_from_slice(&available[..n]);
            self.read_pos += n;
            if self.read_pos == self.read_buf.len() {
                self.read_buf.clear();
                self.read_pos = 0;
            }
            return Ok(n);
        }

        // Receive from dispatcher channel instead of calling bulk_in.
        let timeout = self.read_timeout.unwrap_or(Duration::from_secs(300));
        loop {
            let msg = match self.message_rx.recv_timeout(timeout) {
                Ok(msg) => msg,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "ADB USB read timeout"));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.closed = true;
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "USB dispatcher stopped (device may have disconnected)",
                    ));
                }
            };

            match msg.command {
                A_WRTE => {
                    // Data for our stream — send OKAY ack.
                    {
                        let iface = self.write_interface.lock()
                            .map_err(|_| io::Error::other("write lock poisoned"))?;
                        usb_send_message(&iface, self.ep_out, A_OKAY, self.local_id, self.remote_id, &[])
                            .map_err(flash_error_to_io)?;
                    }
                    self.read_buf = msg.data;
                    self.read_pos = 0;
                    let n = self.read_buf.len().min(buf.len());
                    buf[..n].copy_from_slice(&self.read_buf[..n]);
                    self.read_pos = n;
                    if self.read_pos == self.read_buf.len() {
                        self.read_buf.clear();
                        self.read_pos = 0;
                    }
                    return Ok(n);
                }
                A_CLSE => {
                    self.closed = true;
                    return Ok(0);
                }
                A_OKAY => {
                    // Write ack — not data, skip.
                    continue;
                }
                _ => {
                    // Unexpected — skip.
                    continue;
                }
            }
        }
    }
}

impl io::Write for UsbStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "ADB stream is closed"));
        }

        let chunk_size = std::cmp::min(buf.len(), self.max_payload as usize);
        let chunk = &buf[..chunk_size];

        // Send WRTE through serialized write interface.
        {
            let iface = self.write_interface.lock()
                .map_err(|_| io::Error::other("write lock poisoned"))?;
            usb_send_message(&iface, self.ep_out, A_WRTE, self.local_id, self.remote_id, chunk)
                .map_err(flash_error_to_io)?;
        }

        // Wait for OKAY from our dispatcher channel.
        let timeout = self.read_timeout.unwrap_or(Duration::from_secs(10));
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "ADB USB write timeout waiting for OKAY"));
            }

            let msg = match self.message_rx.recv_timeout(remaining) {
                Ok(msg) => msg,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "ADB USB write timeout waiting for OKAY"));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.closed = true;
                    return Err(io::Error::new(io::ErrorKind::ConnectionReset, "USB dispatcher stopped during write"));
                }
            };

            match msg.command {
                A_OKAY => return Ok(chunk_size),
                A_WRTE => {
                    // Incoming data while waiting for write ack — buffer it, send OKAY.
                    {
                        let iface = self.write_interface.lock()
                            .map_err(|_| io::Error::other("write lock poisoned"))?;
                        usb_send_message(&iface, self.ep_out, A_OKAY, self.local_id, self.remote_id, &[])
                            .map_err(flash_error_to_io)?;
                    }
                    self.read_buf.extend_from_slice(&msg.data);
                    continue;
                }
                A_CLSE => {
                    self.closed = true;
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "ADB stream closed by device during write"));
                }
                _ => continue,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for UsbStream {
    fn drop(&mut self) {
        self.close();
        self.dispatcher.unregister_stream(self.local_id);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: build a complete ADB message (header + payload) in one buffer.
    /// Used for testing parse_adb_message which expects this format.
    fn build_message(command: u32, arg0: u32, arg1: u32, data: &[u8]) -> Vec<u8> {
        let data_length = data.len() as u32;
        let checksum = adb_checksum(data);
        let magic = command ^ 0xFFFF_FFFF;
        let mut buf = Vec::with_capacity(ADB_HEADER_SIZE + data.len());
        buf.extend_from_slice(&command.to_le_bytes());
        buf.extend_from_slice(&arg0.to_le_bytes());
        buf.extend_from_slice(&arg1.to_le_bytes());
        buf.extend_from_slice(&data_length.to_le_bytes());
        buf.extend_from_slice(&checksum.to_le_bytes());
        buf.extend_from_slice(&magic.to_le_bytes());
        buf.extend_from_slice(data);
        buf
    }

    // --- Task 2: ADB USB Message Protocol ---

    #[test]
    fn test_adb_checksum_empty() {
        assert_eq!(adb_checksum(&[]), 0);
    }

    #[test]
    fn test_adb_checksum_data() {
        // 0x01 + 0x02 + 0x03 = 6
        assert_eq!(adb_checksum(&[0x01, 0x02, 0x03]), 6);
        // All 0xFF bytes: 0xFF * 3 = 765
        assert_eq!(adb_checksum(&[0xFF, 0xFF, 0xFF]), 765);
    }

    #[test]
    fn test_build_message_cnxn() {
        let data = b"host::features=shell_v2";
        let msg = build_message(A_CNXN, A_VERSION, MAX_PAYLOAD, data);

        // Total length must be header + data.
        assert_eq!(msg.len(), ADB_HEADER_SIZE + data.len());

        // command field.
        assert_eq!(u32::from_le_bytes(msg[0..4].try_into().unwrap()), A_CNXN);
        // arg0 = A_VERSION.
        assert_eq!(u32::from_le_bytes(msg[4..8].try_into().unwrap()), A_VERSION);
        // arg1 = MAX_PAYLOAD.
        assert_eq!(u32::from_le_bytes(msg[8..12].try_into().unwrap()), MAX_PAYLOAD);
        // data_length.
        assert_eq!(
            u32::from_le_bytes(msg[12..16].try_into().unwrap()),
            data.len() as u32
        );
        // checksum.
        assert_eq!(
            u32::from_le_bytes(msg[16..20].try_into().unwrap()),
            adb_checksum(data)
        );
        // magic = A_CNXN ^ 0xFFFFFFFF.
        assert_eq!(
            u32::from_le_bytes(msg[20..24].try_into().unwrap()),
            A_CNXN ^ 0xFFFF_FFFF
        );
        // Payload bytes.
        assert_eq!(&msg[ADB_HEADER_SIZE..], data);
    }

    #[test]
    fn test_build_message_no_data() {
        let msg = build_message(A_OKAY, 1, 2, &[]);
        assert_eq!(msg.len(), ADB_HEADER_SIZE);
        // data_length = 0.
        assert_eq!(u32::from_le_bytes(msg[12..16].try_into().unwrap()), 0u32);
        // checksum of empty = 0.
        assert_eq!(u32::from_le_bytes(msg[16..20].try_into().unwrap()), 0u32);
        // magic.
        assert_eq!(
            u32::from_le_bytes(msg[20..24].try_into().unwrap()),
            A_OKAY ^ 0xFFFF_FFFF
        );
    }

    #[test]
    fn test_parse_message_valid() {
        // Roundtrip: build then parse.
        let data = b"device::features=shell_v2";
        let raw = build_message(A_CNXN, A_VERSION, MAX_PAYLOAD, data);
        let msg = parse_message(&raw).expect("parse should succeed");

        assert_eq!(msg.command, A_CNXN);
        assert_eq!(msg.arg0, A_VERSION);
        assert_eq!(msg.arg1, MAX_PAYLOAD);
        assert_eq!(msg.data, data);
    }

    #[test]
    fn test_parse_message_truncated() {
        // Buffer shorter than 24 bytes must return Protocol error.
        let raw = &[0u8; 16];
        let result = parse_message(raw);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("too short"), "expected 'too short' in: {}", err);
    }

    #[test]
    fn test_parse_message_bad_magic() {
        let data = b"test";
        let mut raw = build_message(A_WRTE, 1, 2, data);
        // Corrupt the magic field (bytes 20..24).
        raw[20] ^= 0xFF;
        let result = parse_message(&raw);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("bad magic"), "expected 'bad magic' in: {}", err);
    }

    #[test]
    fn test_command_constants() {
        // Verify ASCII encoding matches AOSP adb.h.
        assert_eq!(A_CNXN, u32::from_le_bytes(*b"CNXN"));
        assert_eq!(A_AUTH, u32::from_le_bytes(*b"AUTH"));
        assert_eq!(A_OPEN, u32::from_le_bytes(*b"OPEN"));
        assert_eq!(A_WRTE, u32::from_le_bytes(*b"WRTE"));
        assert_eq!(A_OKAY, u32::from_le_bytes(*b"OKAY"));
        assert_eq!(A_CLSE, u32::from_le_bytes(*b"CLSE"));
    }

    // --- Task 3: CNXN Banner Parsing ---

    #[test]
    fn test_parse_banner_device_with_features() {
        let info = parse_cnxn_banner("device::ro.product.model=SM-G9810;features=shell_v2,cmd,stat_v2");
        assert_eq!(info.state, "device");
        assert!(info.features.contains(&"shell_v2".to_string()));
        assert!(info.features.contains(&"cmd".to_string()));
        assert!(info.features.contains(&"stat_v2".to_string()));
        assert!(info.has_shell_v2);
    }

    #[test]
    fn test_parse_banner_recovery() {
        let info = parse_cnxn_banner("recovery::features=shell_v2");
        assert_eq!(info.state, "recovery");
        assert_eq!(info.features, vec!["shell_v2".to_string()]);
        assert!(info.has_shell_v2);
    }

    #[test]
    fn test_parse_banner_sideload() {
        // Empty features section.
        let info = parse_cnxn_banner("sideload::");
        assert_eq!(info.state, "sideload");
        assert!(info.features.is_empty());
        assert!(!info.has_shell_v2);
    }

    #[test]
    fn test_parse_banner_no_features() {
        // No "features=" key at all.
        let info = parse_cnxn_banner("device::ro.product.name=walleye");
        assert_eq!(info.state, "device");
        assert!(info.features.is_empty());
        assert!(!info.has_shell_v2);
    }

    #[test]
    fn test_parse_banner_null_terminated() {
        // Trailing NUL byte must be stripped.
        let info = parse_cnxn_banner("device::features=shell_v2,cmd\0");
        assert_eq!(info.state, "device");
        assert!(info.features.contains(&"shell_v2".to_string()));
        assert!(info.features.contains(&"cmd".to_string()));
        assert!(info.has_shell_v2);
    }

    #[test]
    fn test_parse_banner_empty() {
        let info = parse_cnxn_banner("");
        assert_eq!(info.state, "unknown");
        assert!(info.features.is_empty());
        assert!(!info.has_shell_v2);
    }

    // --- Task 4: RSA Auth Crypto ---

    #[test]
    fn test_adb_key_path() {
        let path = adb_key_path();
        assert!(path.to_string_lossy().contains("adbkey"));
        assert!(path.to_string_lossy().contains(".android"));
    }

    #[test]
    fn test_sign_token_deterministic() {
        use rsa::rand_core::OsRng;
        use rsa::RsaPrivateKey;

        let key = RsaPrivateKey::new(&mut OsRng, 2048).unwrap();
        let token = [0u8; 20]; // dummy token (same size as SHA-1 digest)

        let sig1 = sign_auth_token(&key, &token).unwrap();
        let sig2 = sign_auth_token(&key, &token).unwrap();

        // PKCS1v15 prehash signing is deterministic.
        assert_eq!(sig1, sig2);
        assert_eq!(sig1.len(), 256); // 2048-bit key = 256-byte signature
    }

    #[test]
    fn test_encode_public_key_format() {
        use rsa::rand_core::OsRng;
        use rsa::RsaPrivateKey;

        let key = RsaPrivateKey::new(&mut OsRng, 2048).unwrap();
        let encoded = encode_android_public_key(&key);

        let text = String::from_utf8_lossy(&encoded);
        assert!(text.contains(' ')); // space before user@host
        assert!(text.ends_with('\0')); // null terminated

        // Base64 part should decode to 524 bytes.
        let b64_part = text.split(' ').next().unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64_part)
            .unwrap();
        assert_eq!(decoded.len(), 524);
    }

    // --- Task 5: USB Bulk I/O Helpers ---

    #[test]
    fn test_build_message_for_send() {
        // Verify that build_message produces a buffer suitable for usb_send_message.
        let data = b"host::features=shell_v2,cmd\0";
        let msg = build_message(A_CNXN, A_VERSION, MAX_PAYLOAD, data);
        assert_eq!(msg.len(), ADB_HEADER_SIZE + data.len());

        // The header should be parseable.
        let parsed = parse_message(&msg).unwrap();
        assert_eq!(parsed.command, A_CNXN);
        assert_eq!(parsed.arg0, A_VERSION);
        assert_eq!(parsed.arg1, MAX_PAYLOAD);
        assert_eq!(parsed.data, data);
    }

    // --- Task 6: USB Connection ---

    #[test]
    fn test_adb_class_constants() {
        assert_eq!(ADB_CLASS, 0xFF);
        assert_eq!(ADB_SUBCLASS, 0x42);
        assert_eq!(ADB_PROTOCOL, 0x01);
    }

    #[test]
    fn test_auth_constants_match_protocol() {
        // AUTH_TOKEN=1 → device sends token challenge
        assert_eq!(AUTH_TOKEN, 1);
        // AUTH_SIGNATURE=2 → host signs token
        assert_eq!(AUTH_SIGNATURE, 2);
        // AUTH_RSAPUBLICKEY=3 → host sends public key for user approval
        assert_eq!(AUTH_RSAPUBLICKEY, 3);
    }

    // --- Task 7: UsbStream ---

    #[test]
    fn test_open_response_routing_logic() {
        let our_local_id: u32 = 42;

        let messages = vec![
            AdbMessage { command: A_WRTE, arg0: 10, arg1: 5, data: b"stale data".to_vec() },
            AdbMessage { command: A_CLSE, arg0: 10, arg1: 5, data: vec![] },
            AdbMessage { command: A_OKAY, arg0: 20, arg1: our_local_id, data: vec![] },
        ];

        let mut result_remote_id = None;
        let mut stale_count = 0u32;
        for msg in &messages {
            match msg.command {
                A_OKAY if msg.arg1 == our_local_id => {
                    result_remote_id = Some(msg.arg0);
                    break;
                }
                A_CLSE if msg.arg1 == our_local_id => {
                    panic!("Should not get CLSE for our stream in this test");
                }
                A_WRTE | A_OKAY | A_CLSE => {
                    stale_count += 1;
                    continue;
                }
                _ => panic!("Unexpected command"),
            }
        }

        assert_eq!(result_remote_id, Some(20), "Should find OKAY for our stream");
        assert_eq!(stale_count, 2, "Should have skipped 2 stale messages");
    }

    #[test]
    fn test_local_id_generator_increments() {
        let id1 = NEXT_LOCAL_ID.load(Ordering::Relaxed);
        let id2 = NEXT_LOCAL_ID.fetch_add(1, Ordering::Relaxed);
        let id3 = NEXT_LOCAL_ID.fetch_add(1, Ordering::Relaxed);
        assert_eq!(id1, id2);
        assert_eq!(id3, id2 + 1);
        // Restore the counter so other tests aren't affected.
        NEXT_LOCAL_ID.fetch_sub(2, Ordering::Relaxed);
    }

    #[test]
    fn test_parse_message_open_service() {
        // Simulate OPEN message for "shell:ls\0"
        let service = b"shell:ls\0";
        let local_id = 42u32;
        let raw = build_message(A_OPEN, local_id, 0, service);
        let msg = parse_message(&raw).unwrap();

        assert_eq!(msg.command, A_OPEN);
        assert_eq!(msg.arg0, 42);
        assert_eq!(msg.arg1, 0);
        assert_eq!(msg.data, service);
    }

    #[test]
    fn test_parse_message_okay() {
        let raw = build_message(A_OKAY, 100, 42, &[]);
        let msg = parse_message(&raw).unwrap();
        assert_eq!(msg.command, A_OKAY);
        assert_eq!(msg.arg0, 100); // remote_id
        assert_eq!(msg.arg1, 42); // local_id
        assert!(msg.data.is_empty());
    }

    #[test]
    fn test_parse_message_wrte() {
        let payload = b"total 0\ndrwxr-xr-x  2 root root 40 Jan  1 00:00 .\n";
        let raw = build_message(A_WRTE, 100, 42, payload);
        let msg = parse_message(&raw).unwrap();
        assert_eq!(msg.command, A_WRTE);
        assert_eq!(msg.data, payload);
    }

    #[test]
    fn test_parse_message_clse() {
        let raw = build_message(A_CLSE, 100, 42, &[]);
        let msg = parse_message(&raw).unwrap();
        assert_eq!(msg.command, A_CLSE);
        assert_eq!(msg.arg0, 100);
        assert_eq!(msg.arg1, 42);
    }

    #[test]
    fn test_stream_id_filtering_logic() {
        let our_local_id: u32 = 50;
        let our_remote_id: u32 = 100;

        let messages = vec![
            AdbMessage { command: A_WRTE, arg0: 10, arg1: 5, data: b"stale".to_vec() },
            AdbMessage { command: A_WRTE, arg0: our_remote_id, arg1: our_local_id, data: b"real data".to_vec() },
        ];

        let mut buffered_data: Option<Vec<u8>> = None;
        let mut stale_count = 0u32;

        for msg in &messages {
            match msg.command {
                A_WRTE if msg.arg1 == our_local_id => {
                    buffered_data = Some(msg.data.clone());
                    break;
                }
                A_WRTE | A_OKAY | A_CLSE => {
                    stale_count += 1;
                    continue;
                }
                _ => panic!("Unexpected"),
            }
        }

        assert_eq!(buffered_data, Some(b"real data".to_vec()));
        assert_eq!(stale_count, 1);
    }

    // --- Dispatcher message routing tests ---

    #[test]
    fn test_dispatcher_routes_by_local_id() {
        // Two streams registered with different local_ids.
        let streams: Arc<Mutex<HashMap<u32, mpsc::Sender<AdbMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let (tx_a, rx_a) = mpsc::channel();
        let (tx_b, rx_b) = mpsc::channel();

        {
            let mut map = streams.lock().unwrap();
            map.insert(1, tx_a);
            map.insert(2, tx_b);
        }

        // Route message to stream A (arg1=1)
        let msg_a = AdbMessage { command: A_WRTE, arg0: 100, arg1: 1, data: vec![0x41] };
        if let Some(tx) = streams.lock().unwrap().get(&msg_a.arg1) {
            tx.send(msg_a).unwrap();
        }

        // Route message to stream B (arg1=2)
        let msg_b = AdbMessage { command: A_WRTE, arg0: 200, arg1: 2, data: vec![0x42] };
        if let Some(tx) = streams.lock().unwrap().get(&msg_b.arg1) {
            tx.send(msg_b).unwrap();
        }

        let received_a = rx_a.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(received_a.arg1, 1);
        assert_eq!(received_a.data, vec![0x41]);

        let received_b = rx_b.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(received_b.arg1, 2);
        assert_eq!(received_b.data, vec![0x42]);
    }

    #[test]
    fn test_dispatcher_discards_unregistered_stream() {
        let streams: Arc<Mutex<HashMap<u32, mpsc::Sender<AdbMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel();
        streams.lock().unwrap().insert(1, tx);

        // Message for unregistered stream 99 — no stream to receive it
        let map = streams.lock().unwrap();
        assert!(map.get(&99).is_none());

        // Stream 1 should have no messages
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_dispatcher_unregister_removes_stream() {
        let streams: Arc<Mutex<HashMap<u32, mpsc::Sender<AdbMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel();
        streams.lock().unwrap().insert(1, tx);
        assert!(streams.lock().unwrap().contains_key(&1));

        streams.lock().unwrap().remove(&1);
        assert!(!streams.lock().unwrap().contains_key(&1));
    }
}
