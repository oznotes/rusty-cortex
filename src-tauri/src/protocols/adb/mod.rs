mod shell;
mod sync;
mod sideload;
mod dump;
mod install;
pub mod logcat;

// Re-exports — maintain backward-compatible public API for commands.rs
pub use shell::{ShellV2Id, ShellOutput, shell_v2_build_packet, shell_v2_read_packet, adb_shell_pub, adb_shell_open, adb_shell_open_command, shell_v2_supported};

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::sync::OnceLock;
use std::time::Duration;

use tauri::{AppHandle, Emitter};
use tokio::task::spawn_blocking;
use tracing::{info, warn};

use crate::error::FlashError;
use crate::types::{AdbState, DeviceInfo, FlashProgress, FlashStage, ProtocolType, RebootMode, RootStatus, RootType};
use super::adb_usb;

/// Default ADB server address.
pub(super) const ADB_SERVER_ADDR: SocketAddrV4 =
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5037);

/// Default read timeout for short ADB shell commands (id, getprop, ls, rm).
pub(super) const ADB_SHELL_TIMEOUT: Option<Duration> = Some(Duration::from_secs(30));

/// Cached check: is `adb` binary available in PATH?
/// Result is cached for the lifetime of the process to avoid spawning on every detect.
fn adb_available() -> bool {
    static ADB_AVAILABLE: OnceLock<bool> = OnceLock::new();
    *ADB_AVAILABLE.get_or_init(|| {
        std::process::Command::new("adb")
            .arg("version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    })
}

pub struct AdbProtocol {
    pub force_usb: bool,
}

// ─── AdbStream transport abstraction ───────────────────────────────────────
// Wraps either a TCP socket (via ADB server) or a direct USB stream.
// Implements Read + Write so all protocol functions work with both transports.

pub enum AdbStream {
    Tcp(TcpStream),
    Usb(adb_usb::UsbStream),
    UsbWriter(adb_usb::UsbShellWriter),
}

impl Read for AdbStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            AdbStream::Tcp(tcp) => tcp.read(buf),
            AdbStream::Usb(usb) => usb.read(buf),
            AdbStream::UsbWriter(_) => Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "UsbWriter is write-only")),
        }
    }
}

impl Write for AdbStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            AdbStream::Tcp(tcp) => tcp.write(buf),
            AdbStream::Usb(usb) => usb.write(buf),
            AdbStream::UsbWriter(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            AdbStream::Tcp(tcp) => tcp.flush(),
            AdbStream::Usb(usb) => usb.flush(),
            AdbStream::UsbWriter(w) => w.flush(),
        }
    }
}

impl AdbStream {
    #[allow(dead_code)] // Called from tests; part of transport API
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            AdbStream::Tcp(tcp) => tcp.set_read_timeout(timeout),
            AdbStream::Usb(usb) => { usb.set_read_timeout(timeout); Ok(()) }
            AdbStream::UsbWriter(_) => Ok(()),
        }
    }

    pub fn shutdown(&mut self) {
        match self {
            AdbStream::Tcp(tcp) => { let _ = tcp.shutdown(std::net::Shutdown::Both); }
            AdbStream::Usb(_) => {} // Drop impl sends CLSE
            AdbStream::UsbWriter(w) => w.shutdown(),
        }
    }
}

// ─── Core ADB transport ────────────────────────────────────────────────────

/// Connect to a device and open an ADB service.
/// Tries direct USB first (with one retry on stale connection), falls back to ADB server (TCP).
pub(super) fn adb_connect(serial: &str, service: &str, read_timeout: Option<Duration>, force_usb: bool) -> Result<AdbStream, FlashError> {
    // Try direct USB — retry once if the cached connection is stale.
    let mut last_usb_err: Option<FlashError> = None;
    for attempt in 0..2u8 {
        match adb_usb::adb_usb_connect(serial) {
            Ok(conn) => {
                // Populate Shell V2 cache from CNXN banner
                shell::shell_v2_cache_insert(serial, conn.banner.has_shell_v2);
                if attempt == 0 {
                    info!("ADB: connected via USB direct to {serial}");
                } else {
                    info!("ADB: reconnected via USB direct to {serial} (fresh connection)");
                }

                match adb_usb::adb_usb_open(conn, service) {
                    Ok(mut stream) => {
                        stream.set_read_timeout(read_timeout);
                        return Ok(AdbStream::Usb(stream));
                    }
                    Err(e) => {
                        warn!("ADB USB OPEN failed (attempt {}): {e}", attempt + 1);
                        adb_usb::invalidate_usb_cache(serial);
                        if attempt == 0 {
                            info!("Retrying with fresh USB connection...");
                            continue;
                        }
                        last_usb_err = Some(e);
                    }
                }
            }
            Err(e) => {
                warn!("ADB USB connect failed (attempt {}): {e}", attempt + 1);
                adb_usb::invalidate_usb_cache(serial);
                if attempt == 0 {
                    continue;
                }
                last_usb_err = Some(e);
            }
        }
    }

    // USB failed after retry
    if force_usb {
        return Err(FlashError::Usb(format!(
            "USB Direct failed: {}",
            last_usb_err.map_or_else(|| "unknown".to_string(), |e| e.to_string())
        )));
    }
    if let Some(ref e) = last_usb_err {
        warn!("ADB USB not available for {serial}: {e}");
    }

    // Fall back to ADB server
    let mut tcp = TcpStream::connect(ADB_SERVER_ADDR)
        .map_err(|e| FlashError::Protocol(format!("ADB server connect failed: {e}")))?;
    tcp.set_read_timeout(read_timeout)
        .map_err(|e| FlashError::Protocol(format!("Failed to set read timeout: {e}")))?;
    let transport = format!("host:transport:{serial}");
    adb_send(&mut tcp, &transport)?;
    adb_send(&mut tcp, service)?;
    info!("ADB: connected via server to {serial}");
    Ok(AdbStream::Tcp(tcp))
}

/// Send a length-prefixed ADB message and read the OKAY/FAIL response.
fn adb_send(stream: &mut TcpStream, msg: &str) -> Result<(), FlashError> {
    if msg.len() > 0xFFFF {
        return Err(FlashError::Protocol(format!("ADB message too long: {} bytes (max 65535)", msg.len())));
    }
    let payload = format!("{:04x}{}", msg.len(), msg);
    stream.write_all(payload.as_bytes())
        .map_err(|e| FlashError::Protocol(format!("ADB write failed: {e}")))?;

    let mut status = [0u8; 4];
    stream.read_exact(&mut status)
        .map_err(|e| FlashError::Protocol(format!("ADB read failed: {e}")))?;

    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let mut len_buf = [0u8; 4];
            if stream.read_exact(&mut len_buf).is_ok() {
                let len = usize::from_str_radix(
                    std::str::from_utf8(&len_buf).unwrap_or("0000"), 16
                ).unwrap_or(0);
                let mut err_buf = vec![0u8; len];
                let _ = stream.read_exact(&mut err_buf);
                let err_msg = String::from_utf8_lossy(&err_buf);
                Err(FlashError::Protocol(format!("ADB server error: {err_msg}")))
            } else {
                Err(FlashError::Protocol("ADB server returned FAIL".into()))
            }
        }
        _ => Err(FlashError::Protocol(format!(
            "Unexpected ADB response: {:?}",
            std::str::from_utf8(&status).unwrap_or("???")
        ))),
    }
}

/// Emit flash progress event to frontend.
pub(super) fn emit_progress(app: &AppHandle, stage: FlashStage, message: &str, percent: Option<f32>) {
    let progress = FlashProgress {
        stage: stage.clone(),
        message: message.to_string(),
        percent,
    };
    let _ = app.emit("flash-progress", &progress);

    match stage {
        FlashStage::Complete | FlashStage::Error => info!("{}", message),
        FlashStage::Sending => {} // progress bar only — no log spam
        _ => info!("{}", message),
    }
}

// ─── AdbProtocol ────────────────────────────────────────────────────────────

impl AdbProtocol {
    pub fn new(force_usb: bool) -> Self {
        Self { force_usb }
    }

    /// Find a device in Sideload or Recovery state for sideloading. BLOCKING.
    pub fn find_sideload_device(force_usb: bool) -> Result<String, FlashError> {
        if force_usb {
            let devices = Self::scan_usb_devices();
            for dev in &devices {
                if let Some(serial) = &dev.serial {
                    if let Ok(conn) = adb_usb::adb_usb_connect(serial) {
                        let state = conn.banner.state.as_str();
                        if state == "sideload" || state == "recovery" {
                            return Ok(serial.clone());
                        }
                    }
                }
            }
            adb_usb::clear_usb_cache();
            Err(FlashError::Protocol(
                "No USB device in sideload mode. Enter Recovery and select 'Apply update from ADB'".into()
            ))
        } else {
            if !adb_available() {
                return Err(FlashError::Protocol(
                    "ADB server unavailable (adb not in PATH). Enable USB Direct mode.".into()
                ));
            }
            let mut server = adb_client::server::ADBServer::new(ADB_SERVER_ADDR);
            let devices = server.devices()
                .map_err(|e| FlashError::Protocol(format!("ADB server connection failed: {e}")))?;

            let sideload = devices.iter()
                .find(|d| d.state == adb_client::server::DeviceState::Sideload);
            let recovery = devices.iter()
                .find(|d| d.state == adb_client::server::DeviceState::Recovery);

            match sideload.or(recovery) {
                Some(d) => Ok(d.identifier.clone()),
                None => Err(FlashError::Protocol(
                    "No device in sideload mode. Enter Recovery and select 'Apply update from ADB'".into()
                )),
            }
        }
    }

    /// Find the serial of the first usable ADB device via USB only (no ADB server). BLOCKING.
    pub fn find_usb_device_serial() -> Result<String, FlashError> {
        let devices = Self::scan_usb_devices();
        devices.into_iter()
            .find_map(|d| d.serial)
            .ok_or(FlashError::NoDevice)
    }

    /// Find the serial of the first usable ADB device. BLOCKING.
    /// Routes to USB-only scan or ADB server depending on `force_usb`.
    pub fn find_serial(force_usb: bool) -> Result<String, FlashError> {
        if force_usb {
            Self::find_usb_device_serial()
        } else {
            Self::find_device_serial()
        }
    }

    /// Find the serial of the first usable ADB device via ADB server. BLOCKING.
    pub fn find_device_serial() -> Result<String, FlashError> {
        if !adb_available() {
            return Err(FlashError::Protocol(
                "ADB server unavailable (adb not in PATH). Enable USB Direct mode.".into()
            ));
        }
        let mut server = adb_client::server::ADBServer::new(ADB_SERVER_ADDR);
        let devices = server.devices()
            .map_err(|e| FlashError::Protocol(format!("ADB server connection failed: {e}")))?;

        devices.into_iter()
            .find(|d| d.state == adb_client::server::DeviceState::Device
                   || d.state == adb_client::server::DeviceState::Recovery
                   || d.state == adb_client::server::DeviceState::Sideload)
            .map(|d| d.identifier)
            .ok_or(FlashError::NoDevice)
    }

    /// Get a crate-managed device handle for operations the crate supports. BLOCKING.
    fn get_device_sync() -> Result<adb_client::server_device::ADBServerDevice, FlashError> {
        if !adb_available() {
            return Err(FlashError::Protocol(
                "ADB server unavailable (adb not in PATH). Enable USB Direct mode.".into()
            ));
        }
        let mut server = adb_client::server::ADBServer::new(ADB_SERVER_ADDR);
        let serial = Self::find_device_serial()?;
        server.get_device_by_name(&serial)
            .map_err(|e| FlashError::Protocol(format!("ADB device connection failed: {e}")))
    }

    async fn get_device() -> Result<adb_client::server_device::ADBServerDevice, FlashError> {
        spawn_blocking(Self::get_device_sync)
            .await
            .map_err(|e| FlashError::Protocol(format!("ADB task failed: {e}")))?
    }

    /// Scan for ADB devices via USB only (no ADB server). BLOCKING.
    pub fn scan_usb_devices() -> Vec<DeviceInfo> {
        let devices = match nusb::list_devices() {
            Ok(d) => d,
            Err(_) => return vec![],
        };

        let mut result = Vec::new();
        for dev_info in devices {
            let serial = match dev_info.serial_number() {
                Some(s) => s.to_string(),
                None => continue,
            };
            let vid = dev_info.vendor_id();
            let pid = dev_info.product_id();
            let manufacturer = dev_info.manufacturer_string().map(str::to_string);
            let product = dev_info.product_string().map(str::to_string);
            if let Ok(device) = dev_info.open() {
                if adb_usb::find_adb_from_config(&device).is_some() {
                    result.push(DeviceInfo {
                        vendor_id: vid,
                        product_id: pid,
                        serial: Some(serial),
                        manufacturer,
                        product,
                        protocol: ProtocolType::Adb,
                        adb_state: Some(AdbState::Normal),
                    });
                }
            }
        }
        result
    }

    /// Query ADB server for connected devices. BLOCKING.
    /// Skips if adb binary is not in PATH (avoids error spam from adb_client crate).
    pub fn scan_adb_server() -> Vec<DeviceInfo> {
        if !adb_available() {
            tracing::debug!("ADB not in PATH, skipping server scan");
            return Vec::new();
        }
        let mut server = adb_client::server::ADBServer::new(ADB_SERVER_ADDR);

        let short_devices = match server.devices() {
            Ok(devices) => devices,
            Err(e) => {
                tracing::debug!("ADB server scan skipped: {e}");
                return Vec::new();
            }
        };

        short_devices
            .into_iter()
            .filter(|d| d.state == adb_client::server::DeviceState::Device
                     || d.state == adb_client::server::DeviceState::Recovery
                     || d.state == adb_client::server::DeviceState::Sideload)
            .map(|d| {
                let adb_state = Some(match d.state {
                    adb_client::server::DeviceState::Recovery => AdbState::Recovery,
                    adb_client::server::DeviceState::Sideload => AdbState::Sideload,
                    _ => AdbState::Normal,
                });
                DeviceInfo {
                    vendor_id: 0,
                    product_id: 0,
                    serial: Some(d.identifier),
                    manufacturer: None,
                    product: None,
                    protocol: ProtocolType::Adb,
                    adb_state,
                }
            })
            .collect()
    }

    /// Parse `getprop` output into key-value pairs.
    fn parse_getprop(output: &str) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for line in output.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix('[') {
                if let Some((key, val)) = rest.split_once("]: [") {
                    if let Some(val) = val.strip_suffix(']') {
                        map.insert(key.to_string(), val.to_string());
                    }
                }
            }
        }
        map
    }

    /// Format bytes to human-readable size string.
    pub fn format_size(bytes: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = 1024 * 1024;
        const GB: u64 = 1024 * 1024 * 1024;
        if bytes >= GB {
            format!("{:.1} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.1} MB", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.1} KB", bytes as f64 / KB as f64)
        } else {
            format!("{} B", bytes)
        }
    }

    /// Check root access on a device. BLOCKING.
    pub fn check_root_sync(serial: &str, force_usb: bool) -> RootStatus {
        if let Ok(result) = shell::adb_shell(serial, "id", ADB_SHELL_TIMEOUT, force_usb) {
            if result.stdout.contains("uid=0(root)") {
                return RootStatus {
                    root_type: RootType::Adb,
                    message: "ADB running as root".into(),
                };
            }
        }

        if let Ok(result) = shell::adb_shell(serial, "su -c id", ADB_SHELL_TIMEOUT, force_usb) {
            if result.stdout.contains("uid=0(root)") {
                return RootStatus {
                    root_type: RootType::Su,
                    message: "Root available via su".into(),
                };
            }
        }

        RootStatus {
            root_type: RootType::None,
            message: "Root access not available".into(),
        }
    }

    /// Collect device health data (battery, storage, RAM). BLOCKING.
    /// Runs a single batched shell command — no root required.
    pub fn get_health_sync(serial: &str, force_usb: bool) -> crate::types::DeviceHealth {
        let cmd = "echo '---BATTERY---' && dumpsys battery 2>/dev/null && echo '---STORAGE---' && df /data 2>/dev/null && echo '---MEMORY---' && cat /proc/meminfo 2>/dev/null";
        let output = match shell::adb_shell(serial, cmd, ADB_SHELL_TIMEOUT, force_usb) {
            Ok(result) => result.stdout,
            Err(e) => {
                warn!("Health check failed: {e}");
                return crate::types::DeviceHealth {
                    battery_level: None,
                    battery_health: None,
                    battery_temp: None,
                    storage_used_gb: None,
                    storage_total_gb: None,
                    ram_used_gb: None,
                    ram_total_gb: None,
                };
            }
        };

        let sections: Vec<&str> = output.split("---").collect();

        // Parse battery section
        let mut battery_level = None;
        let mut battery_health = None;
        let mut battery_temp = None;
        for section in &sections {
            if !section.contains("level:") { continue; }
            for line in section.lines() {
                let line = line.trim();
                if let Some(val) = line.strip_prefix("level:") {
                    battery_level = val.trim().parse::<u32>().ok();
                } else if let Some(val) = line.strip_prefix("health:") {
                    battery_health = Some(match val.trim() {
                        "2" => "Good".to_string(),
                        "3" => "Overheat".to_string(),
                        "4" => "Dead".to_string(),
                        "5" => "Over voltage".to_string(),
                        "6" => "Failure".to_string(),
                        "7" => "Cold".to_string(),
                        other => format!("Unknown ({other})"),
                    });
                } else if let Some(val) = line.strip_prefix("temperature:") {
                    if let Ok(raw) = val.trim().parse::<f32>() {
                        battery_temp = Some(raw / 10.0);
                    }
                }
            }
            break;
        }

        // Parse storage section (df /data output)
        let mut storage_used_gb = None;
        let mut storage_total_gb = None;
        for section in &sections {
            if !section.contains("STORAGE") { continue; }
            // Next section has the df output
            if let Some(next_idx) = sections.iter().position(|s| s.contains("STORAGE")) {
                if let Some(df_section) = sections.get(next_idx + 1) {
                    for line in df_section.lines().skip(1) {
                        let fields: Vec<&str> = line.split_whitespace().collect();
                        if fields.len() >= 4 {
                            // df output: Filesystem 1K-blocks Used Available ...
                            if let (Ok(total_kb), Ok(used_kb)) = (
                                fields[1].parse::<u64>(),
                                fields[2].parse::<u64>(),
                            ) {
                                storage_total_gb = Some(total_kb as f32 / 1_048_576.0);
                                storage_used_gb = Some(used_kb as f32 / 1_048_576.0);
                            }
                            break;
                        }
                    }
                }
            }
            break;
        }

        // Parse memory section
        let mut ram_total_gb = None;
        let mut ram_used_gb = None;
        let mut mem_total_kb: Option<u64> = None;
        let mut mem_available_kb: Option<u64> = None;
        for section in &sections {
            if !section.contains("MemTotal") { continue; }
            for line in section.lines() {
                let line = line.trim();
                if line.starts_with("MemTotal:") {
                    mem_total_kb = line.split_whitespace().nth(1).and_then(|v| v.parse().ok());
                } else if line.starts_with("MemAvailable:") {
                    mem_available_kb = line.split_whitespace().nth(1).and_then(|v| v.parse().ok());
                }
            }
            break;
        }
        if let Some(total) = mem_total_kb {
            ram_total_gb = Some(total as f32 / 1_048_576.0);
            if let Some(available) = mem_available_kb {
                ram_used_gb = Some((total - available) as f32 / 1_048_576.0);
            }
        }

        info!("Health: battery={}%, temp={:?}°C, storage={:?}/{:?}GB, ram={:?}/{:?}GB",
            battery_level.unwrap_or(0), battery_temp,
            storage_used_gb, storage_total_gb,
            ram_used_gb, ram_total_gb);

        crate::types::DeviceHealth {
            battery_level,
            battery_health,
            battery_temp,
            storage_used_gb,
            storage_total_gb,
            ram_used_gb,
            ram_total_gb,
        }
    }

    /// Wrap a shell command for the appropriate root type.
    /// Escapes single quotes in the command to prevent shell injection via su -c.
    pub fn wrap_for_root(cmd: &str, root_type: &RootType) -> String {
        match root_type {
            RootType::Su => {
                let escaped = cmd.replace('\'', "'\\''");
                format!("su -c '{escaped}'")
            }
            _ => cmd.to_string(),
        }
    }

    /// Validate a path used in shell commands contains no injection characters.
    pub(super) fn validate_shell_path(path: &str) -> Result<(), FlashError> {
        if path.is_empty() {
            return Err(FlashError::Validation("Path is empty".into()));
        }
        const FORBIDDEN: &[char] = &[';', '|', '&', '$', '`', '(', ')', '{', '}', '\n', '\r', '\\', '\'', '"'];
        if let Some(bad) = path.chars().find(|c| FORBIDDEN.contains(c)) {
            return Err(FlashError::Validation(format!(
                "Path contains unsafe character '{bad}': {path}"
            )));
        }
        Ok(())
    }

    /// List directory contents on device. BLOCKING.
    pub fn list_directory_sync(serial: &str, path: &str, force_usb: bool) -> Result<Vec<(String, bool, String)>, FlashError> {
        Self::validate_shell_path(path)?;
        let output = shell::adb_shell(serial, &format!("ls -la {path}"), ADB_SHELL_TIMEOUT, force_usb)?.stdout;
        let mut entries = Vec::new();

        for line in output.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("total") {
                continue;
            }
            let fields: Vec<&str> = line.splitn(8, char::is_whitespace).collect();
            if fields.len() < 7 {
                continue;
            }
            let perms = fields[0];
            let name = fields.last().unwrap_or(&"").trim();

            if name == "." || name == ".." || name.is_empty() {
                continue;
            }

            let is_dir = perms.starts_with('d');
            let size = if is_dir {
                "".to_string()
            } else {
                fields.get(4).and_then(|s| s.parse::<u64>().ok())
                    .map(Self::format_size)
                    .unwrap_or_default()
            };

            entries.push((name.to_string(), is_dir, size));
        }

        entries.sort_by(|a, b| {
            b.1.cmp(&a.1).then(a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });

        Ok(entries)
    }
}

impl Default for AdbProtocol {
    fn default() -> Self {
        Self::new(false)
    }
}

impl AdbProtocol {
    #[allow(dead_code)]
    pub async fn detect(&self) -> Result<Option<DeviceInfo>, FlashError> {
        shell::shell_v2_clear_cache();
        let force_usb = self.force_usb;
        let devices = spawn_blocking(move || {
            if force_usb {
                Self::scan_usb_devices()
            } else {
                Self::scan_adb_server()
            }
        })
        .await
        .map_err(|e| FlashError::Protocol(format!("ADB scan task failed: {e}")))?;
        Ok(devices.into_iter().next())
    }

    pub async fn get_partitions(&self) -> Result<Vec<String>, FlashError> {
        Ok(Vec::new())
    }

    #[allow(dead_code)]
    pub async fn get_var(&self, name: &str) -> Result<String, FlashError> {
        let vars = self.get_all_vars().await?;
        vars.get(name)
            .cloned()
            .ok_or_else(|| FlashError::Protocol(format!("Property not found: {name}")))
    }

    pub async fn get_all_vars(&self) -> Result<HashMap<String, String>, FlashError> {
        let force_usb = self.force_usb;
        spawn_blocking(move || {
            let serial = Self::find_serial(force_usb)?;
            let output = shell::adb_shell(&serial, "getprop", ADB_SHELL_TIMEOUT, force_usb)?.stdout;
            Ok(Self::parse_getprop(&output))
        })
        .await
        .map_err(|e| FlashError::Protocol(format!("getprop task failed: {e}")))?
    }

    pub async fn reboot(&self, mode: RebootMode) -> Result<(), FlashError> {
        shell::shell_v2_clear_cache();

        // EDL reboot via shell command — send through existing connection first,
        // then clear cache (same pattern as non-EDL USB Direct path).
        if mode == RebootMode::Edl {
            info!("ADB: Rebooting to EDL...");
            let force_usb = self.force_usb;
            return spawn_blocking(move || {
                let serial = Self::find_serial(force_usb)?;
                let result = match shell::adb_shell(&serial, "reboot edl", ADB_SHELL_TIMEOUT, force_usb) {
                    Ok(output) => {
                        info!("EDL reboot command sent: {}", output.stdout.trim());
                        Ok(())
                    }
                    Err(FlashError::DeviceDisconnected) => {
                        info!("EDL reboot command sent, device disconnecting (expected)");
                        Ok(())
                    }
                    Err(FlashError::Usb(msg)) if msg.contains("closed") || msg.contains("disconnect") || msg.contains("Disconnect") || msg.contains("dispatcher stopped") => {
                        info!("EDL reboot command sent, device disconnecting (expected)");
                        Ok(())
                    }
                    Err(FlashError::Protocol(msg)) if msg.contains("closed") || msg.contains("disconnect") || msg.contains("dispatcher stopped") => {
                        info!("EDL reboot command sent, device disconnecting (expected)");
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                adb_usb::clear_usb_cache();
                result
            })
            .await
            .map_err(|e| FlashError::Protocol(format!("EDL reboot task failed: {e}")))?;
        }

        if self.force_usb {
            let force_usb = self.force_usb;
            let reboot_service = match mode {
                RebootMode::Normal => {
                    info!("ADB: Rebooting device (USB Direct)...");
                    "reboot:"
                }
                RebootMode::Bootloader => {
                    info!("ADB: Rebooting to bootloader (USB Direct)...");
                    "reboot:bootloader"
                }
                RebootMode::Recovery => {
                    info!("ADB: Rebooting to recovery (USB Direct)...");
                    "reboot:recovery"
                }
                RebootMode::Edl => {
                    return Err(FlashError::Protocol("Internal error: EDL reboot should be handled before USB Direct path".into()));
                }
            };
            let service = reboot_service.to_string();
            return spawn_blocking(move || {
                let serial = Self::find_serial(force_usb)?;
                let result = match adb_connect(&serial, &service, Some(Duration::from_secs(5)), force_usb) {
                    Ok(_) => Ok(()),
                    Err(FlashError::DeviceDisconnected) => {
                        info!("Reboot command sent, device disconnecting (expected)");
                        Ok(())
                    }
                    Err(FlashError::Protocol(msg)) if msg.contains("closed") || msg.contains("disconnect") => {
                        info!("Reboot command sent, device disconnecting (expected)");
                        Ok(())
                    }
                    Err(FlashError::Usb(msg)) if msg.contains("closed") || msg.contains("disconnect") || msg.contains("Disconnect") => {
                        info!("Reboot command sent, device disconnecting (expected)");
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                adb_usb::clear_usb_cache();
                result
            })
            .await
            .map_err(|e| FlashError::Protocol(format!("Reboot task failed: {e}")))?;
        }

        // ADB server fallback (force_usb=false)
        adb_usb::clear_usb_cache();
        let mut device = Self::get_device().await?;
        let reboot_type = match mode {
            RebootMode::Normal => {
                info!("ADB: Rebooting device...");
                adb_client::RebootType::System
            }
            RebootMode::Bootloader => {
                info!("ADB: Rebooting to bootloader...");
                adb_client::RebootType::Bootloader
            }
            RebootMode::Recovery => {
                info!("ADB: Rebooting to recovery...");
                adb_client::RebootType::Recovery
            }
            RebootMode::Edl => {
                return Err(FlashError::Protocol("Internal error: EDL reboot should be handled before ADB server path".into()));
            }
        };

        spawn_blocking(move || {
            device.reboot(reboot_type)
                .map_err(|e| FlashError::Protocol(format!("ADB reboot failed: {e}")))
        })
        .await
        .map_err(|e| FlashError::Protocol(format!("ADB reboot task failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adb_protocol_force_usb() {
        let usb_on = AdbProtocol::new(true);
        assert!(usb_on.force_usb);

        let usb_off = AdbProtocol::new(false);
        assert!(!usb_off.force_usb);
    }

    #[test]
    fn test_parse_getprop_standard() {
        let input = "[ro.product.model]: [Pixel 6]\n[ro.build.display.id]: [TP1A.220624.014]\n";
        let map = AdbProtocol::parse_getprop(input);
        assert_eq!(map.get("ro.product.model").unwrap(), "Pixel 6");
        assert_eq!(map.get("ro.build.display.id").unwrap(), "TP1A.220624.014");
    }

    #[test]
    fn test_parse_getprop_empty_value() {
        let input = "[ro.some.prop]: []\n";
        let map = AdbProtocol::parse_getprop(input);
        assert_eq!(map.get("ro.some.prop").unwrap(), "");
    }

    #[test]
    fn test_parse_getprop_malformed_lines() {
        let input = "not a prop line\n[valid.key]: [valid_value]\ngarbage\n";
        let map = AdbProtocol::parse_getprop(input);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("valid.key").unwrap(), "valid_value");
    }

    #[test]
    fn test_adb_send_format() {
        let msg = "host:transport:445a34fc";
        let payload = format!("{:04x}{}", msg.len(), msg);
        assert_eq!(payload, "0017host:transport:445a34fc");
    }

    #[test]
    fn test_parse_root_uid_zero() {
        let output = "uid=0(root) gid=0(root) groups=0(root) context=u:r:su:s0\n";
        assert!(output.contains("uid=0"));
    }

    #[test]
    fn test_parse_root_non_root() {
        let output = "uid=2000(shell) gid=2000(shell) groups=2000(shell),1004(input)\n";
        assert!(!output.contains("uid=0(root)"));
    }

    #[test]
    fn test_format_size_display() {
        assert_eq!(AdbProtocol::format_size(67108864), "64.0 MB");
        assert_eq!(AdbProtocol::format_size(4509715660), "4.2 GB");
        assert_eq!(AdbProtocol::format_size(1048576), "1.0 MB");
        assert_eq!(AdbProtocol::format_size(512), "512 B");
        assert_eq!(AdbProtocol::format_size(1024), "1.0 KB");
    }

    #[test]
    fn test_validate_shell_path_safe() {
        assert!(AdbProtocol::validate_shell_path("/dev/block/by-name/boot").is_ok());
        assert!(AdbProtocol::validate_shell_path("/data/local/tmp").is_ok());
        assert!(AdbProtocol::validate_shell_path("/sdcard/Download/file.img").is_ok());
        assert!(AdbProtocol::validate_shell_path("/mnt/user/0/my folder").is_ok());
    }

    #[test]
    fn test_validate_shell_path_unsafe() {
        assert!(AdbProtocol::validate_shell_path("").is_err());
        assert!(AdbProtocol::validate_shell_path("/dev/block;rm -rf /").is_err());
        assert!(AdbProtocol::validate_shell_path("$(whoami)").is_err());
        assert!(AdbProtocol::validate_shell_path("/tmp/`id`").is_err());
        assert!(AdbProtocol::validate_shell_path("/tmp|cat /etc/passwd").is_err());
        assert!(AdbProtocol::validate_shell_path("/tmp&echo pwned").is_err());
    }

    #[test]
    fn test_wrap_with_su() {
        let cmd = "dd if=/dev/block/by-name/boot of=/sdcard/boot.img bs=4096";
        assert_eq!(
            AdbProtocol::wrap_for_root(cmd, &RootType::Su),
            "su -c 'dd if=/dev/block/by-name/boot of=/sdcard/boot.img bs=4096'"
        );
        assert_eq!(
            AdbProtocol::wrap_for_root(cmd, &RootType::Adb),
            "dd if=/dev/block/by-name/boot of=/sdcard/boot.img bs=4096"
        );
    }

    #[test]
    fn test_wrap_for_root_single_quotes() {
        let cmd = "for p in $(ls /dev/block/by-name/); do echo $p; done";
        let wrapped = AdbProtocol::wrap_for_root(cmd, &RootType::Su);
        assert!(wrapped.starts_with("su -c '"));
        assert!(wrapped.ends_with("'"));
        assert!(wrapped.contains("$p"));
    }
}
