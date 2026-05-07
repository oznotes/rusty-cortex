use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, State};
use tracing::{info, warn};

use crate::device::detect;
use crate::flash::manager;
use crate::flash::validation;
use crate::protocols::adb::{AdbProtocol, AdbStream, ShellOutput, ShellV2Id, adb_shell_open, adb_shell_open_command, adb_shell_pub, shell_v2_read_packet, shell_v2_build_packet};
use crate::protocols::adb::logcat::{LogcatParser, LogcatFilter, LogPriority};
use crate::protocols::edl::EdlProtocol;
use crate::protocols::fastboot::FastbootProtocol;
use crate::protocols::edl_db::{ProgrammerDatabase, scan_programmers, score_candidates};
use crate::types::{DeviceInfo, EdlDeviceInfo, EdlPartitionEntry, BatchFlashResult, RawprogramDiscovery, ProgrammerEntry, ProgrammerCandidate, VerifyResult, ProtocolType, RebootMode, RootStatus};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ShellSession {
    pub writer: Arc<Mutex<AdbStream>>,
    pub is_v2: bool,
}

pub struct AppState {
    pub current_device: Mutex<Option<DeviceInfo>>,
    pub is_flashing: Mutex<bool>,
    pub force_usb: Mutex<bool>,
    pub shell_sessions: Mutex<HashMap<String, ShellSession>>,
    pub edl_connection: Mutex<Option<crate::protocols::edl::EdlConnection>>,
    pub programmer_db: Mutex<ProgrammerDatabase>,
    pub edl_identify_cache: Mutex<Option<(String, EdlDeviceInfo)>>,
    /// Sahara transport kept alive after identify() so connect() can reuse it.
    /// The pending HELLO from the last SWITCH_MODE is in the buffer.
    pub edl_sahara_device: Mutex<Option<qdl::types::QdlDevice<dyn qdl::types::QdlReadWrite>>>,
    /// Cache of parsed programmer identities keyed by (path, file_size).
    /// Avoids re-parsing on repeated scans of the same folder.
    pub programmer_identity_cache: Mutex<HashMap<(String, u64), crate::types::ProgrammerIdentity>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            current_device: Mutex::new(None),
            is_flashing: Mutex::new(false),
            force_usb: Mutex::new(false),
            shell_sessions: Mutex::new(HashMap::new()),
            edl_connection: Mutex::new(None),
            programmer_db: Mutex::new(ProgrammerDatabase::empty()),
            edl_identify_cache: Mutex::new(None),
            edl_sahara_device: Mutex::new(None),
            programmer_identity_cache: Mutex::new(HashMap::new()),
        }
    }
}

/// RAII guard that resets `is_flashing` to `false` on drop.
/// Ensures the flag is always reset, even on panic or early return.
struct FlashGuard<'a> {
    flag: &'a Mutex<bool>,
}

impl<'a> FlashGuard<'a> {
    fn try_acquire(flag: &'a Mutex<bool>) -> Result<Self, String> {
        let mut is_flashing = flag.lock()
            .map_err(|e| format!("State lock failed: {e}"))?;
        if *is_flashing {
            return Err("Another operation is in progress".into());
        }
        *is_flashing = true;
        Ok(FlashGuard { flag })
    }
}

impl Drop for FlashGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut f) = self.flag.lock() {
            *f = false;
        }
    }
}

#[tauri::command]
pub async fn detect_device(state: State<'_, AppState>) -> Result<Option<DeviceInfo>, String> {
    info!("Scanning for devices...");

    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;

    // Kill ADB server early — give it maximum time to release USB before connect.
    // This runs BEFORE the scan so the server has the entire scan duration to die.
    if force_usb {
        tokio::task::spawn_blocking(|| {
            crate::protocols::adb_usb::kill_adb_server();
        }).await.ok();
    }

    let scan_result = tokio::time::timeout(
        Duration::from_secs(8),
        tokio::task::spawn_blocking(move || {
            if force_usb {
                // USB-only path: do NOT touch ADB server, which would auto-start
                // adb.exe and grab the USB interface before nusb can use it.
                // scan_devices() now detects ADB/Fastboot by USB interface descriptor
                // (class 0xFF/0x42, protocol 0x01=ADB, 0x03=Fastboot) — no VID/PID
                // ambiguity. EDL/MTK fall back to VID/PID table.
                detect::scan_devices().unwrap_or_default()
            } else {
                let mut devices = detect::scan_devices().unwrap_or_default();

                // Also scan ADB server for devices not found via USB
                let adb_devices = AdbProtocol::scan_adb_server();
                for adb_dev in adb_devices {
                    let dominated = devices.iter().any(|d| d.serial == adb_dev.serial);
                    if !dominated {
                        devices.push(adb_dev);
                    }
                }

                devices
            }
        }),
    )
    .await;

    let devices = match scan_result {
        Ok(Ok(devices)) => devices,
        Ok(Err(e)) => return Err(format!("Scan task failed: {}", e)),
        Err(_) => return Err("Device scan timed out after 8 seconds".into()),
    };

    let device = devices.into_iter().next();
    *state.current_device.lock().map_err(|e| format!("State lock failed: {}", e))? = device.clone();

    // Clear EDL identify cache and stale Sahara transport when device changes.
    // Cache is keyed by Sahara serial; invalidate when no EDL device is present.
    if let Ok(mut cache) = state.edl_identify_cache.lock() {
        if cache.is_some() {
            let still_edl = device.as_ref()
                .map(|d| d.protocol == crate::types::ProtocolType::Edl)
                .unwrap_or(false);
            if !still_edl {
                info!("EDL: clearing identify cache (device changed)");
                *cache = None;
                // Also clear stale Sahara transport
                if let Ok(mut dev) = state.edl_sahara_device.lock() {
                    *dev = None;
                }
            }
        }
    }

    if let Some(ref d) = device {
        info!(
            "Found device: {} ({:04x}:{:04x}) - {}",
            d.product.as_deref().unwrap_or("Unknown"),
            d.vendor_id,
            d.product_id,
            d.protocol
        );
    } else {
        info!("No device found");
    }

    Ok(device)
}

#[tauri::command]
pub async fn get_partitions(protocol: ProtocolType, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;

    let result = match protocol {
        ProtocolType::Fastboot => {
            let fb = FastbootProtocol::new();
            tokio::time::timeout(COMMAND_TIMEOUT, fb.get_partitions()).await
        }
        ProtocolType::Adb => {
            let adb = AdbProtocol::new(force_usb);
            tokio::time::timeout(COMMAND_TIMEOUT, adb.get_partitions()).await
        }
        ProtocolType::Edl => {
            return Err("Use edl_identify/edl_connect for EDL operations".into())
        }
        ProtocolType::MtkBrom => {
            return Err(format!("Unsupported protocol: {}", protocol))
        }
    };

    match result {
        Ok(Ok(parts)) => Ok(parts),
        Ok(Err(e)) => {
            warn!("Partition query failed: {}", e);
            Err(e.to_string())
        }
        Err(_) => {
            warn!("Partition query timed out");
            Err("Partition query timed out".into())
        }
    }
}

#[tauri::command]
pub async fn flash_firmware(
    app: AppHandle,
    state: State<'_, AppState>,
    firmware_path: String,
    partition: String,
) -> Result<(), String> {
    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;

    let path = PathBuf::from(&firmware_path);
    let protocol = FastbootProtocol::new();
    // Flash operations get a generous timeout — large firmware can take minutes.
    const FLASH_TIMEOUT: Duration = Duration::from_secs(600);
    match tokio::time::timeout(FLASH_TIMEOUT, manager::run_flash(&app, &protocol, &path, &partition)).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(_) => Err("Flash operation timed out after 10 minutes".into()),
    }
}

#[tauri::command]
pub async fn check_critical_partition(partition: String) -> Result<bool, String> {
    Ok(validation::is_critical_partition(&partition))
}

#[tauri::command]
pub async fn get_device_vars(protocol: ProtocolType, state: State<'_, AppState>) -> Result<HashMap<String, String>, String> {
    info!("Querying device variables...");
    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;

    let result = match protocol {
        ProtocolType::Fastboot => {
            let fb = FastbootProtocol::new();
            tokio::time::timeout(COMMAND_TIMEOUT, fb.get_all_vars()).await
        }
        ProtocolType::Adb => {
            let adb = AdbProtocol::new(force_usb);
            tokio::time::timeout(COMMAND_TIMEOUT, adb.get_all_vars()).await
        }
        ProtocolType::Edl => {
            return Err("Use edl_identify/edl_connect for EDL operations".into())
        }
        ProtocolType::MtkBrom => {
            return Err(format!("Unsupported protocol: {}", protocol))
        }
    };

    match result {
        Ok(Ok(vars)) => {
            info!("Got {} variables from device", vars.len());
            Ok(vars)
        }
        Ok(Err(e)) => {
            warn!("Device variable query failed: {}", e);
            Err(e.to_string())
        }
        Err(_) => {
            warn!("Device variable query timed out");
            Err("Device variable query timed out".into())
        }
    }
}

#[tauri::command]
pub async fn reboot_device(mode: RebootMode, state: State<'_, AppState>) -> Result<(), String> {
    // Close all shell sessions before rebooting — releases USB interface Arc refs
    // so the dispatcher can actually shut down when the cache is cleared.
    {
        let mut sessions = state.shell_sessions.lock()
            .map_err(|e| format!("State lock failed: {e}"))?;
        for (sid, session) in sessions.drain() {
            let _ = session.writer.lock().map(|mut s| s.shutdown());
            info!("Shell session closed before reboot: {}", sid);
        }
    }

    let protocol_type = {
        let device = state.current_device.lock().map_err(|e| format!("State lock failed: {e}"))?;
        device.as_ref().map(|d| d.protocol.clone())
    };

    let result = match protocol_type {
        Some(ProtocolType::Fastboot) => {
            let protocol = FastbootProtocol::new();
            tokio::time::timeout(COMMAND_TIMEOUT, protocol.reboot(mode))
                .await
                .map_err(|_| "Reboot timed out".to_string())?
                .map_err(|e| e.to_string())
        }
        Some(ProtocolType::Adb) => {
            let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;
            let protocol = AdbProtocol::new(force_usb);
            tokio::time::timeout(COMMAND_TIMEOUT, protocol.reboot(mode))
                .await
                .map_err(|_| "Reboot timed out".to_string())?
                .map_err(|e| e.to_string())
        }
        _ => Err("No device connected or unsupported protocol".into()),
    };

    // Clear stale device state on success — device is rebooting to a different mode.
    // Prevents wrong protocol routing if user clicks buttons before re-detecting.
    if result.is_ok() {
        *state.current_device.lock().map_err(|e| format!("State lock failed: {e}"))? = None;
    }

    result
}

#[tauri::command]
pub async fn sideload_firmware(
    app: AppHandle,
    state: State<'_, AppState>,
    firmware_path: String,
) -> Result<(), String> {
    info!("Sideload requested: {}", firmware_path);

    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;

    // Validate file
    let path = PathBuf::from(&firmware_path);
    if !path.exists() {
        return Err(format!("File not found: {}", firmware_path));
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("zip") => {}
        _ => return Err("Sideload requires a ZIP file".into()),
    }

    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;

    // Find device in sideload or recovery state
    let serial = tokio::task::spawn_blocking(move || {
        AdbProtocol::find_sideload_device(force_usb)
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?
    .map_err(|e| e.to_string())?;

    info!("Sideload target device: {}", serial);

    // Run sideload transfer (blocking — raw TCP)
    let app_clone = app.clone();
    tokio::task::spawn_blocking(move || {
        AdbProtocol::run_sideload(&serial, &path, &app_clone, force_usb)
    })
    .await
    .map_err(|e| format!("Sideload task failed: {e}"))?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn push_file(
    app: AppHandle,
    state: State<'_, AppState>,
    local_path: String,
    remote_path: String,
) -> Result<(), String> {
    info!("Push requested: {} -> {}", local_path, remote_path);

    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;

    let path = PathBuf::from(&local_path);
    if !path.exists() {
        return Err(format!("File not found: {}", local_path));
    }

    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;

    let serial = tokio::task::spawn_blocking(move || {
        AdbProtocol::find_serial(force_usb)
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?
    .map_err(|e| e.to_string())?;

    let app_clone = app.clone();
    tokio::task::spawn_blocking(move || {
        AdbProtocol::run_push(&serial, &path, &remote_path, &app_clone, force_usb)
    })
    .await
    .map_err(|e| format!("Push task failed: {e}"))?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn pull_file(
    app: AppHandle,
    state: State<'_, AppState>,
    remote_path: String,
    local_path: String,
) -> Result<(), String> {
    info!("Pull requested: {} -> {}", remote_path, local_path);

    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;

    let path = PathBuf::from(&local_path);

    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;

    let serial = tokio::task::spawn_blocking(move || {
        AdbProtocol::find_serial(force_usb)
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?
    .map_err(|e| e.to_string())?;

    let app_clone = app.clone();
    tokio::task::spawn_blocking(move || {
        AdbProtocol::run_pull(&serial, &remote_path, &path, &app_clone, force_usb)
    })
    .await
    .map_err(|e| format!("Pull task failed: {e}"))?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn shell_open(
    app: AppHandle,
    state: State<'_, AppState>,
    serial: String,
    session_id: String,
    on_output: Channel<ShellOutput>,
) -> Result<(), String> {
    info!("Shell open requested: session={}, serial={}", session_id, serial);

    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;
    let serial_clone = serial.clone();
    let (reader, writer, is_v2) = tokio::task::spawn_blocking(move || {
        adb_shell_open(&serial_clone, force_usb)
    })
    .await
    .map_err(|e| format!("Shell open task failed: {e}"))?
    .map_err(|e| e.to_string())?;

    let writer = Arc::new(Mutex::new(writer));
    {
        let mut sessions = state.shell_sessions.lock()
            .map_err(|e| format!("Session lock failed: {e}"))?;
        if let Some(old) = sessions.remove(&session_id) {
            let _ = old.writer.lock().map(|mut s| s.shutdown());
        }
        sessions.insert(session_id.clone(), ShellSession { writer, is_v2 });
    }

    let sid = session_id.clone();
    let app_clone = app.clone();
    tokio::task::spawn_blocking(move || {
        let mut reader = reader;

        if is_v2 {
            // Shell V2: read packets, route by type
            loop {
                match shell_v2_read_packet(&mut reader) {
                    Ok((ShellV2Id::Stdout, data)) => {
                        if on_output.send(ShellOutput::Data { data }).is_err() {
                            break;
                        }
                    }
                    Ok((ShellV2Id::Stderr, data)) => {
                        if on_output.send(ShellOutput::Stderr { data }).is_err() {
                            break;
                        }
                    }
                    Ok((ShellV2Id::Exit, data)) => {
                        let code = data.first().copied();
                        let _ = on_output.send(ShellOutput::Exit {
                            message: "Session ended".into(),
                            code,
                        });
                        break;
                    }
                    Ok(_) => {} // Ignore other packets
                    Err(e) => {
                        let msg = format!("Connection lost: {e}");
                        let _ = on_output.send(ShellOutput::Exit { message: msg, code: None });
                        break;
                    }
                }
            }
        } else {
            // Shell V1: raw byte reader
            use std::io::Read;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = on_output.send(ShellOutput::Exit {
                            message: "Session ended".into(),
                            code: None,
                        });
                        break;
                    }
                    Ok(n) => {
                        if on_output.send(ShellOutput::Data {
                            data: buf[..n].to_vec(),
                        }).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let msg = if e.kind() == std::io::ErrorKind::ConnectionReset
                            || e.kind() == std::io::ErrorKind::ConnectionAborted {
                            "Device disconnected".to_string()
                        } else {
                            format!("Connection lost: {e}")
                        };
                        let _ = on_output.send(ShellOutput::Exit { message: msg, code: None });
                        break;
                    }
                }
            }
        }

        // Auto-cleanup
        if let Some(app_state) = app_clone.try_state::<AppState>() {
            let _ = app_state.shell_sessions.lock().map(|mut s| { s.remove(&sid); });
        }
        info!("Shell reader exited: session={}", sid);
    });

    Ok(())
}

#[tauri::command]
pub async fn shell_write(
    state: State<'_, AppState>,
    session_id: String,
    data: Vec<u8>,
) -> Result<(), String> {
    let (writer, is_v2) = {
        let sessions = state.shell_sessions.lock()
            .map_err(|e| format!("Session lock failed: {e}"))?;
        let session = sessions.get(&session_id)
            .ok_or_else(|| "Shell session not found".to_string())?;
        (session.writer.clone(), session.is_v2)
    };

    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut stream = writer.lock()
            .map_err(|e| format!("Writer lock failed: {e}"))?;

        if is_v2 {
            // Shell V2: wrap in stdin packet
            let packet = shell_v2_build_packet(ShellV2Id::Stdin, &data);
            stream.write_all(&packet)
                .map_err(|e| format!("Shell V2 write failed: {e}"))
        } else {
            // Shell V1: raw bytes
            stream.write_all(&data)
                .map_err(|e| format!("Shell write failed: {e}"))
        }
    })
    .await
    .map_err(|e| format!("Shell write task failed: {e}"))?
}

#[tauri::command]
pub async fn shell_close(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    let session = {
        let mut sessions = state.shell_sessions.lock()
            .map_err(|e| format!("Session lock failed: {e}"))?;
        sessions.remove(&session_id)
    };
    if let Some(session) = session {
        let _ = session.writer.lock()
            .map(|mut s| s.shutdown());
        info!("Shell session closed: {}", session_id);
    }
    Ok(())
}

#[tauri::command]
pub async fn shell_resize(
    state: State<'_, AppState>,
    session_id: String,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    let (writer, is_v2) = {
        let sessions = state.shell_sessions.lock()
            .map_err(|e| format!("Session lock failed: {e}"))?;
        let session = sessions.get(&session_id)
            .ok_or_else(|| "Shell session not found".to_string())?;
        (session.writer.clone(), session.is_v2)
    };

    if !is_v2 {
        return Ok(()); // V1 has no resize support
    }

    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut stream = writer.lock()
            .map_err(|e| format!("Writer lock failed: {e}"))?;

        // Binary winsize struct: 4x u16 LE (rows, cols, xpixel, ypixel)
        let mut resize_data = Vec::with_capacity(8);
        resize_data.extend_from_slice(&rows.to_le_bytes());
        resize_data.extend_from_slice(&cols.to_le_bytes());
        resize_data.extend_from_slice(&0u16.to_le_bytes());
        resize_data.extend_from_slice(&0u16.to_le_bytes());
        let packet = shell_v2_build_packet(ShellV2Id::WindowSizeChange, &resize_data);
        stream.write_all(&packet)
            .map_err(|e| format!("Shell resize failed: {e}"))
    })
    .await
    .map_err(|e| format!("Shell resize task failed: {e}"))?
}

#[tauri::command]
pub async fn check_root(state: State<'_, AppState>) -> Result<RootStatus, String> {
    info!("Checking root access...");
    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;
    let result = tokio::task::spawn_blocking(move || {
        let serial = AdbProtocol::find_serial(force_usb)
            .map_err(|e| e.to_string())?;
        Ok(AdbProtocol::check_root_sync(&serial, force_usb))
    })
    .await
    .map_err(|e| format!("Root check task failed: {e}"))?;
    result
}

#[tauri::command]
pub async fn get_device_health(state: State<'_, AppState>) -> Result<crate::types::DeviceHealth, String> {
    info!("Fetching device health...");
    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;
    tokio::task::spawn_blocking(move || {
        let serial = AdbProtocol::find_serial(force_usb)
            .map_err(|e| e.to_string())?;
        Ok(AdbProtocol::get_health_sync(&serial, force_usb))
    })
    .await
    .map_err(|e| format!("Health check task failed: {e}"))?
}

#[tauri::command]
pub async fn list_partitions_dump(state: State<'_, AppState>) -> Result<crate::types::DumpListResult, String> {
    info!("Listing partitions for dump...");
    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;
    tokio::task::spawn_blocking(move || {
        let serial = AdbProtocol::find_serial(force_usb)
            .map_err(|e| e.to_string())?;
        let root = AdbProtocol::check_root_sync(&serial, force_usb);
        if root.root_type == crate::types::RootType::None {
            return Err("Root access required to list partitions".into());
        }
        let partitions = AdbProtocol::list_partitions_sync(&serial, &root.root_type, force_usb)
            .map_err(|e| e.to_string())?;
        let temp_dir = AdbProtocol::find_writable_temp(&serial, &root.root_type, force_usb);
        let free_bytes = AdbProtocol::check_device_space(&serial, &temp_dir, force_usb);
        let supports_shell_v2 = crate::protocols::adb::shell_v2_supported(&serial, force_usb);
        Ok(crate::types::DumpListResult {
            partitions,
            temp_dir,
            free_bytes,
            supports_shell_v2,
        })
    })
    .await
    .map_err(|e| format!("Partition list task failed: {e}"))?
}

#[tauri::command]
pub async fn dump_partitions(
    app: AppHandle,
    state: State<'_, AppState>,
    partitions: Vec<String>,
    output_dir: String,
    temp_dir: String,
) -> Result<(), String> {
    info!("Dump requested: {} partitions to {}", partitions.len(), output_dir);

    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;
    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;

    let dir = PathBuf::from(&output_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create output dir: {e}"))?;

    let serial = tokio::task::spawn_blocking(move || {
        AdbProtocol::find_serial(force_usb)
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?
    .map_err(|e| e.to_string())?;

    let root = tokio::task::spawn_blocking({
        let serial = serial.clone();
        move || AdbProtocol::check_root_sync(&serial, force_usb)
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?;

    if root.root_type == crate::types::RootType::None {
        return Err("Root access required".into());
    }

    let total = partitions.len();
    for (i, partition) in partitions.iter().enumerate() {
        let local_path = dir.join(format!("{partition}.img"));
        let msg = format!("Reading {} ({}/{})", partition, i + 1, total);
        info!("{}", msg);

        let serial = serial.clone();
        let partition = partition.clone();
        let partition_name = partition.clone();
        let root_type = root.root_type.clone();
        let app_clone = app.clone();
        let label = format!("{} ({}/{})", partition, i + 1, total);
        let temp_dir = temp_dir.clone();

        tokio::task::spawn_blocking(move || {
            AdbProtocol::run_dump_partition(
                &serial, &partition, &local_path, &root_type, &temp_dir, &app_clone, &label, force_usb,
            )
        })
        .await
        .map_err(|e| format!("Dump task failed: {e}"))?
        .map_err(|e| format!("Failed to dump {}: {}", partition_name, e))?;
    }

    let progress = crate::types::FlashProgress {
        stage: crate::types::FlashStage::Complete,
        message: format!("Dumped {} partitions successfully!", total),
        percent: Some(100.0),
    };
    let _ = app.emit("flash-progress", &progress);
    Ok(())
}

#[tauri::command]
pub async fn dump_image(
    app: AppHandle,
    state: State<'_, AppState>,
    device: String,
    offset: u64,
    size: Option<u64>,
    local_path: String,
    temp_dir: String,
) -> Result<(), String> {
    info!("Image dump requested: {} offset={} size={:?}", device, offset, size);

    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;
    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;

    let serial = tokio::task::spawn_blocking(move || {
        AdbProtocol::find_serial(force_usb)
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?
    .map_err(|e| e.to_string())?;

    let root = tokio::task::spawn_blocking({
        let serial = serial.clone();
        move || AdbProtocol::check_root_sync(&serial, force_usb)
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?;

    if root.root_type == crate::types::RootType::None {
        return Err("Root access required".into());
    }

    let path = PathBuf::from(&local_path);
    let offset_opt = if offset > 0 { Some(offset) } else { None };
    let app_clone = app.clone();
    let root_type = root.root_type.clone();

    tokio::task::spawn_blocking(move || {
        AdbProtocol::run_dump_image(
            &serial, &device, offset_opt, size, &path, &root_type, &temp_dir, &app_clone, force_usb,
        )
    })
    .await
    .map_err(|e| format!("Dump task failed: {e}"))?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn write_partition(
    app: AppHandle,
    state: State<'_, AppState>,
    partition: String,
    image_path: String,
    temp_dir: String,
) -> Result<(), String> {
    info!("Write requested: {} -> partition {}", image_path, partition);

    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;
    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;

    let path = PathBuf::from(&image_path);
    if !path.exists() {
        return Err(format!("File not found: {}", image_path));
    }

    let serial = tokio::task::spawn_blocking(move || {
        AdbProtocol::find_serial(force_usb)
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?
    .map_err(|e| e.to_string())?;

    let root = tokio::task::spawn_blocking({
        let serial = serial.clone();
        move || AdbProtocol::check_root_sync(&serial, force_usb)
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?;

    if root.root_type == crate::types::RootType::None {
        return Err("Root access required for partition write".into());
    }

    let serial_clone = serial.clone();
    let partition_clone = partition.clone();
    let app_clone = app.clone();

    tokio::task::spawn_blocking(move || {
        AdbProtocol::run_write_partition(
            &serial_clone, &partition_clone, &path, &root.root_type,
            &temp_dir, &app_clone, &partition_clone, force_usb,
        )
    })
    .await
    .map_err(|e| format!("Write task failed: {e}"))?
    .map_err(|e| format!("Failed to write {}: {}", partition, e))?;

    let progress = crate::types::FlashProgress {
        stage: crate::types::FlashStage::Complete,
        message: format!("Partition {} written successfully!", partition),
        percent: Some(100.0),
    };
    let _ = app.emit("flash-progress", &progress);
    Ok(())
}

#[tauri::command]
pub async fn install_apk(
    app: AppHandle,
    state: State<'_, AppState>,
    apk_path: String,
    replace: bool,
    downgrade: bool,
    grant_all: bool,
) -> Result<(), String> {
    info!("Install APK requested: {} (replace={}, downgrade={}, grant_all={})",
        apk_path, replace, downgrade, grant_all);

    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;

    let path = PathBuf::from(&apk_path);
    if !path.exists() {
        return Err(format!("File not found: {}", apk_path));
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("apk") => {}
        _ => return Err("File must be an APK (.apk)".into()),
    }

    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;

    let serial = tokio::task::spawn_blocking(move || {
        AdbProtocol::find_serial(force_usb)
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?
    .map_err(|e| e.to_string())?;

    let app_clone = app.clone();
    tokio::task::spawn_blocking(move || {
        AdbProtocol::run_install(&serial, &path, replace, downgrade, grant_all, &app_clone, force_usb)
    })
    .await
    .map_err(|e| format!("Install task failed: {e}"))?
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn adb_local_command(state: State<'_, AppState>, args: String) -> Result<String, String> {
    info!("ADB local command: {}", args);
    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;
    tokio::task::spawn_blocking(move || {
        let serial = AdbProtocol::find_serial(force_usb)
            .map_err(|e| e.to_string())?;

        let first_word = args.split_whitespace().next().unwrap_or("");
        let output = match first_word {
            "devices" => {
                let mut out = "List of devices attached\n".to_string();
                // Always include the currently connected device (works with USB Direct)
                out.push_str(&format!("{}\tdevice\n", serial));
                // Also include ADB server devices if server is reachable
                let server_devices = AdbProtocol::scan_adb_server();
                for d in &server_devices {
                    let s = d.serial.as_deref().unwrap_or("unknown");
                    if s == serial { continue; } // skip duplicate
                    let state_str = match d.adb_state {
                        Some(crate::types::AdbState::Recovery) => "recovery",
                        Some(crate::types::AdbState::Sideload) => "sideload",
                        _ => "device",
                    };
                    out.push_str(&format!("{}\t{}\n", s, state_str));
                }
                out
            }
            "shell" => {
                let cmd = args.strip_prefix("shell").unwrap_or("").trim();
                if cmd.is_empty() {
                    return Err("Usage: adb shell <command>".into());
                }
                adb_shell_pub(&serial, cmd, Some(Duration::from_secs(30)), force_usb)
                    .map_err(|e| e.to_string())?.stdout
            }
            _ => {
                // Passthrough as shell command — only from adb-prefixed terminal input
                let trimmed = args.trim();
                if trimmed.is_empty() {
                    return Err("Empty command".into());
                }
                adb_shell_pub(&serial, trimmed, Some(Duration::from_secs(30)), force_usb)
                    .map_err(|e| e.to_string())?.stdout
            }
        };
        Ok(output)
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?
}

#[tauri::command]
pub async fn list_device_directory(state: State<'_, AppState>, path: String) -> Result<Vec<(String, bool, String)>, String> {
    let force_usb = *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?;
    tokio::task::spawn_blocking(move || {
        let serial = AdbProtocol::find_serial(force_usb)
            .map_err(|e| e.to_string())?;
        AdbProtocol::list_directory_sync(&serial, &path, force_usb)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("Task failed: {e}"))?
}

/// Returns which of the given file paths already exist on the host filesystem.
/// Used by the UI to warn before overwriting dump output files.
#[tauri::command]
pub fn check_files_exist(paths: Vec<String>) -> Vec<String> {
    paths
        .into_iter()
        .filter(|p| PathBuf::from(p).exists())
        .collect()
}

/// Checks which partitions need to be dumped by comparing local file sizes
/// against expected device partition sizes. Returns partition names that are
/// missing or incomplete (file doesn't exist or size mismatch).
#[tauri::command]
pub fn check_dump_resume(
    output_dir: String,
    partitions: Vec<(String, Option<u64>)>,
) -> Vec<String> {
    let dir = PathBuf::from(output_dir);
    partitions
        .into_iter()
        .filter_map(|(name, expected_size)| {
            // Defensive: reject path traversal in partition names
            if name.contains("..") || name.contains('/') || name.contains('\\') {
                return Some(name);
            }
            let path = dir.join(format!("{name}.img"));
            if !path.exists() {
                return Some(name);
            }
            // If we know the expected size, verify it matches
            if let Some(expected) = expected_size {
                let actual = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if actual != expected {
                    return Some(name);
                }
            }
            // File exists and size matches (or size unknown) — skip it
            None
        })
        .collect()
}

#[tauri::command]
pub fn set_usb_mode(state: State<'_, AppState>, force_usb: bool) -> Result<(), String> {
    *state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))? = force_usb;
    info!("USB mode set to: {}", if force_usb { "USB Direct" } else { "Auto (USB + server fallback)" });
    Ok(())
}

#[tauri::command]
pub fn get_usb_mode(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(*state.force_usb.lock().map_err(|e| format!("State lock failed: {e}"))?)
}

// --- EDL commands ---

#[tauri::command]
pub async fn edl_identify(state: State<'_, AppState>) -> Result<EdlDeviceInfo, String> {
    // Check cache — if an EDL device is still present and cache is valid, return it.
    // Cache is keyed by Sahara serial; detect_device already invalidates on device change.
    if let Ok(cache) = state.edl_identify_cache.lock() {
        if let Some((cached_serial, cached_info)) = cache.as_ref() {
            if let Ok(dev) = state.current_device.lock() {
                if let Some(ref device) = *dev {
                    if device.protocol == crate::types::ProtocolType::Edl {
                        info!("EDL: returning cached identify result for {cached_serial}");
                        return Ok(cached_info.clone());
                    }
                }
            }
        }
    }

    info!("EDL: identifying device via Sahara...");
    let (mut info, sahara_device) = tokio::task::spawn_blocking(EdlProtocol::identify)
        .await
        .map_err(|e| format!("EDL identify task failed: {e}"))?
        .map_err(|e| e.to_string())?;

    // When resuming Firehose (no Sahara serial), extract serial from USB descriptor.
    // EDL devices have serial=None but product="QUSB_BULK_CID:xxxx_SN:6A6FF601"
    if info.firehose_active && info.serial.is_none() {
        if let Ok(dev) = state.current_device.lock() {
            if let Some(ref device) = *dev {
                // Try serial field first, then parse from product name (SN:XXXXXXXX)
                let sn = device.serial.clone().or_else(|| {
                    device.product.as_ref().and_then(|p| {
                        p.find("SN:").map(|i| p[i + 3..].split('_').next().unwrap_or("").to_string())
                    }).filter(|s| !s.is_empty())
                });
                if let Some(serial) = sn {
                    info!("EDL: using USB descriptor serial for Firehose recovery: {serial}");
                    info.serial = Some(serial);
                }
            }
        }
    }

    // Resolve chipset name from HWID (Sahara raw hex → MSM_ID → human name).
    // Sahara HWID is 8 bytes in raw LE order. Interpret as LE u64; the upper 32 bits
    // are the MSM_ID (e.g., "00007200e1500a00" → u64 0x000a50e100720000 → MSM_ID 0x000A50E1).
    if info.chipset.is_none() {
        if let Some(ref hex_str) = info.hw_id.clone() {
            if let Ok(bytes) = hex::decode(hex_str) {
                if bytes.len() >= 8 {
                    let val = u64::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3],
                        bytes[4], bytes[5], bytes[6], bytes[7],
                    ]);
                    let msm_id = (val >> 32) as u32;
                    if let Some(name) = crate::protocols::edl_mbn::resolve_chipset(msm_id) {
                        info!("EDL: resolved chipset {} for MSM_ID 0x{:08X}", name, msm_id);
                        info.chipset = Some(name.to_string());
                    }
                }
            }
        }
    }

    // Store transport for connect() reuse if either:
    // - Sahara responded (normal flow: pending HELLO in buffer)
    // - Firehose nop probe succeeded (stale session: transport is live Firehose)
    let sahara_responded = info.serial.is_some() || info.hw_id.is_some() || info.pk_hash.is_some();

    if sahara_responded || info.firehose_active {
        if let Ok(mut dev) = state.edl_sahara_device.lock() {
            if dev.is_some() {
                info!("EDL: dropping previous Sahara transport (re-identify)");
            }
            *dev = Some(sahara_device);
            info!("EDL: transport stored for connect reuse");
        }
    } else {
        // Drop the transport — neither Sahara nor Firehose responded, serial port must be freed
        drop(sahara_device);
        warn!("EDL: Sahara timed out, nop probe failed — transport released (device needs power cycle)");
        return Err("Device state unknown — EDL device is not responding to Sahara or Firehose commands. \
                    Power cycle the device (disconnect battery, wait 5 seconds, reconnect) and try again.".into());
    }

    // Cache keyed by Sahara serial (more reliable than USB serial for EDL devices).
    // When firehose_active (Sahara timed out but Firehose detected), use synthetic key
    // so the firehose_active flag survives to edl_connect.
    let cache_key = if let Some(ref s) = info.serial {
        Some(s.clone())
    } else if info.firehose_active {
        Some("firehose-recovery".to_string())
    } else {
        None
    };
    if let Some(key) = cache_key {
        if let Ok(mut cache) = state.edl_identify_cache.lock() {
            *cache = Some((key.clone(), info.clone()));
            info!("EDL: cached identify result for {key}");
        }
    }

    Ok(info)
}

#[tauri::command]
pub async fn edl_connect(
    state: State<'_, AppState>,
    app_handle: AppHandle,
    programmer_path: String,
    hwid: Option<String>,
    pkhash: Option<String>,
) -> Result<EdlDeviceInfo, String> {
    info!("EDL: connecting with programmer: {programmer_path}");

    // Check if already connected — don't try to open a second connection
    match state.edl_connection.lock() {
        Ok(conn) => {
            if conn.is_some() {
                warn!("EDL: connect rejected — already connected. Disconnect first.");
                return Err("EDL device is already connected. Disconnect first before reconnecting.".into());
            }
            info!("EDL: no existing connection, proceeding with connect");
        }
        Err(e) => {
            warn!("EDL: connection lock poisoned: {e}");
            return Err("Internal error: connection state corrupted. Restart the app.".into());
        }
    }

    let path = PathBuf::from(&programmer_path);

    // Check if device is in Firehose mode from a previous session
    let firehose_active = state.edl_identify_cache.lock()
        .ok()
        .and_then(|cache| cache.as_ref().map(|(_, info)| info.firehose_active))
        .unwrap_or(false);

    // Take the stored Sahara transport from identify (has pending HELLO in buffer,
    // or is a live Firehose session if firehose_active)
    let sahara_device = state.edl_sahara_device.lock()
        .ok()
        .and_then(|mut dev| dev.take());

    let mut connection = tokio::task::spawn_blocking(move || EdlProtocol::connect(&path, sahara_device, firehose_active))
        .await
        .map_err(|e| format!("EDL connect task failed: {e}"))?
        .map_err(|e| e.to_string())?;

    // Merge identify info (serial/hwid/pkhash) into connection info.
    // connect() only gets storage_type/sector_size/num_luns from the programmer.
    // The identity fields come from Sahara identify, cached earlier.
    if let Ok(cache) = state.edl_identify_cache.lock() {
        if let Some((_, ref cached_info)) = *cache {
            if connection.info.serial.is_none() {
                connection.info.serial = cached_info.serial.clone();
            }
            if connection.info.hw_id.is_none() {
                connection.info.hw_id = cached_info.hw_id.clone();
            }
            if connection.info.pk_hash.is_none() {
                connection.info.pk_hash = cached_info.pk_hash.clone();
            }
            if connection.info.chipset.is_none() {
                connection.info.chipset = cached_info.chipset.clone();
            }
        }
    }

    let info = connection.info.clone();
    *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = Some(connection);

    // Auto-save to programmer database on successful connect
    if let (Some(hw), Some(pk)) = (&hwid, &pkhash) {
        if let Err(e) = ensure_db_loaded(&state, &app_handle) {
            warn!("DB load failed, skipping save: {e}");
        } else if let Ok(mut db) = state.programmer_db.lock() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let entry = ProgrammerEntry {
                programmer_path: programmer_path.clone(),
                programmer_name: PathBuf::from(&programmer_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string(),
                device_serial: info.serial.clone(),
                storage_type: info.storage_type.clone(),
                last_used: format!("{now}"),
                use_count: 1,
                file_exists: true, // file was just used — it definitely exists now
            };
            db.add(hw, pk, entry);
            if let Err(e) = db.save() {
                warn!("Programmer DB save failed: {e}");
            } else {
                info!("Programmer DB: saved mapping for {}:{}", hw, pk);
            }
        }
    }

    Ok(info)
}

// EDL commands that access edl_connection need special handling:
// The MutexGuard is not Send, so we take() the connection, pass it to
// spawn_blocking, then put it back. This avoids blocking the Tokio runtime.
//
// IMPORTANT: If spawn_blocking panics (qdlrs parser), the connection is lost
// inside the panic. We detect this and clear AppState so the user gets a clean
// "not connected" error instead of a hang.

#[tauri::command]
pub async fn edl_list_partitions(
    state: State<'_, AppState>,
    lun: u8,
) -> Result<Vec<EdlPartitionEntry>, String> {
    info!("EDL: listing partitions for LUN {lun}");
    let conn = state.edl_connection.lock()
        .map_err(|e| format!("State lock failed: {e}"))?
        .take()
        .ok_or_else(|| "EDL not connected. Upload a programmer first.".to_string())?;

    let result = tokio::task::spawn_blocking(move || {
        let mut conn = conn;
        let parts = EdlProtocol::list_partitions(&mut conn, lun);
        (conn, parts)
    })
    .await;

    match result {
        Ok((conn, parts)) => {
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = Some(conn);
            parts.map_err(|e| e.to_string())
        }
        Err(e) => {
            warn!("EDL: list_partitions panicked, connection lost: {e}");
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = None;
            Err(format!("EDL operation failed (internal error). Reconnect required. {e}"))
        }
    }
}

#[tauri::command]
pub async fn edl_read_partition(
    state: State<'_, AppState>,
    app: AppHandle,
    lun: u8,
    partition_name: String,
    start_sector: u64,
    num_sectors: u64,
    output_path: String,
) -> Result<(), String> {
    info!("EDL: reading partition '{}' (LUN {}, sectors {}..+{}) to {}",
        partition_name, lun, start_sector, num_sectors, output_path);

    let conn = state.edl_connection.lock()
        .map_err(|e| format!("State lock failed: {e}"))?
        .take()
        .ok_or_else(|| "EDL not connected. Upload a programmer first.".to_string())?;

    // Calculate total bytes for progress
    let sector_size = conn.info.sector_size.unwrap_or(4096) as u64;
    let total_bytes = num_sectors * sector_size;

    let path = PathBuf::from(&output_path);
    let app_clone = app.clone();

    let result = tokio::task::spawn_blocking(move || {
        let mut conn = conn;

        let file = std::fs::File::create(&path)
            .map_err(|e| crate::error::FlashError::Protocol(format!("Cannot create output file: {e}")));
        let file = match file {
            Ok(f) => f,
            Err(e) => return (conn, Err(e)),
        };
        let buf_writer = std::io::BufWriter::new(file);

        let (mut progress_writer, written_counter) =
            crate::protocols::edl::ProgressWriter::new(buf_writer);

        // Spawn a progress reporter thread that polls the counter
        let app_for_progress = app_clone;
        let total = total_bytes;
        let counter = written_counter;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_clone = done.clone();
        let progress_handle = std::thread::spawn(move || {
            let mut last_pct: u64 = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                if done_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let written = counter.load(std::sync::atomic::Ordering::Relaxed);
                let pct = if total > 0 { (written * 100) / total } else { 0 };
                if pct > last_pct || written >= total {
                    last_pct = pct;
                    let mb_written = written / 1_048_576;
                    let mb_total = total / 1_048_576;
                    let _ = app_for_progress.emit("flash-progress", crate::types::FlashProgress {
                        stage: crate::types::FlashStage::Sending,
                        message: format!("Reading... {} / {} MB", mb_written, mb_total),
                        percent: Some(pct.min(100) as f32),
                    });
                }
                if total > 0 && written >= total {
                    break;
                }
            }
        });

        let r = EdlProtocol::read_partition_to_writer(
            &mut conn, lun, start_sector, num_sectors, &mut progress_writer,
        );

        // Signal progress thread to exit
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = progress_handle.join();

        info!("EDL: saved to {}", path.display());
        (conn, r)
    })
    .await;

    match result {
        Ok((conn, r)) => {
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = Some(conn);
            r.map_err(|e| e.to_string())
        }
        Err(e) => {
            warn!("EDL: read_partition panicked, connection lost: {e}");
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = None;
            Err(format!("EDL operation failed (internal error). Reconnect required. {e}"))
        }
    }
}

#[tauri::command]
pub async fn edl_reboot(
    state: State<'_, AppState>,
    mode: String,
) -> Result<(), String> {
    info!("EDL: rebooting (mode: {mode})");
    let conn = state.edl_connection.lock()
        .map_err(|e| format!("State lock failed: {e}"))?
        .take()
        .ok_or_else(|| "EDL not connected.".to_string())?;

    let result = tokio::task::spawn_blocking(move || {
        let mut conn = conn;
        EdlProtocol::reboot(&mut conn, &mode)
    })
    .await;

    // Clear connection and Sahara transport after reboot (whether it succeeded or panicked)
    *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = None;
    *state.edl_sahara_device.lock().map_err(|e| format!("State lock failed: {e}"))? = None;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => {
            warn!("EDL: reboot panicked: {e}");
            Err(format!("EDL reboot failed (internal error): {e}"))
        }
    }
}

#[tauri::command]
pub async fn edl_disconnect(state: State<'_, AppState>) -> Result<(), String> {
    info!("EDL: disconnecting");
    *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = None;
    *state.edl_sahara_device.lock().map_err(|e| format!("State lock failed: {e}"))? = None;
    // Also clear identify cache to force re-query on next connect
    if let Ok(mut cache) = state.edl_identify_cache.lock() {
        *cache = None;
    }
    Ok(())
}

#[tauri::command]
pub async fn edl_program_partition(
    state: State<'_, AppState>,
    app: AppHandle,
    lun: u8,
    partition_name: String,
    start_sector: u64,
    num_sectors: u64,
    file_path: String,
) -> Result<Option<VerifyResult>, String> {
    info!(
        "EDL: writing '{}' to partition '{}' (LUN {}, sectors {}..+{})",
        file_path, partition_name, lun, start_sector, num_sectors
    );

    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;

    let conn = state
        .edl_connection
        .lock()
        .map_err(|e| format!("State lock failed: {e}"))?
        .take()
        .ok_or_else(|| "EDL not connected. Upload a programmer first.".to_string())?;

    // Get file size for progress (use raw file size as approximation)
    let total_bytes = std::fs::metadata(&file_path).map(|m| m.len()).unwrap_or(0);

    let path = PathBuf::from(&file_path);
    let app_clone = app.clone();

    let result = tokio::task::spawn_blocking(move || {
        let mut conn = conn;

        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter_clone = counter.clone();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_clone = done.clone();

        // Spawn progress reporter thread (exits when done flag set or total reached)
        let total = total_bytes;
        let progress_handle = std::thread::spawn(move || {
            let mut last_pct: u64 = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                if done_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let read = counter_clone.load(std::sync::atomic::Ordering::Relaxed);
                let pct = if total > 0 { (read * 100) / total } else { 0 };
                if pct > last_pct || read >= total {
                    last_pct = pct;
                    let mb_read = read / 1_048_576;
                    let mb_total = total / 1_048_576;
                    let _ = app_clone.emit("flash-progress", crate::types::FlashProgress {
                        stage: crate::types::FlashStage::Sending,
                        message: format!("Writing... {} / {} MB", mb_read, mb_total),
                        percent: Some(pct.min(100) as f32),
                    });
                }
                if total > 0 && read >= total {
                    break;
                }
            }
        });

        let r = EdlProtocol::program_partition(
            &mut conn, lun, start_sector, num_sectors, &path, Some(counter),
        );

        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = progress_handle.join();
        (conn, r)
    })
    .await;

    match result {
        Ok((conn, r)) => {
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = Some(conn);
            r.map_err(|e| e.to_string())
        }
        Err(e) => {
            warn!("EDL: program_partition panicked, connection lost: {e}");
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = None;
            Err(format!("EDL operation failed (internal error). Reconnect required. {e}"))
        }
    }
}

#[tauri::command]
pub async fn edl_erase_partition(
    state: State<'_, AppState>,
    lun: u8,
    partition_name: String,
    start_sector: u64,
    num_sectors: u64,
) -> Result<(), String> {
    info!(
        "EDL: erasing partition '{}' (LUN {}, sectors {}..+{})",
        partition_name, lun, start_sector, num_sectors
    );

    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;

    let conn = state
        .edl_connection
        .lock()
        .map_err(|e| format!("State lock failed: {e}"))?
        .take()
        .ok_or_else(|| "EDL not connected. Upload a programmer first.".to_string())?;

    let result = tokio::task::spawn_blocking(move || {
        let mut conn = conn;
        let r = EdlProtocol::erase_partition(&mut conn, lun, start_sector, num_sectors);
        (conn, r)
    })
    .await;

    match result {
        Ok((conn, r)) => {
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = Some(conn);
            r.map_err(|e| e.to_string())
        }
        Err(e) => {
            warn!("EDL: erase_partition panicked, connection lost: {e}");
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = None;
            Err(format!("EDL operation failed (internal error). Reconnect required. {e}"))
        }
    }
}

#[tauri::command]
pub async fn edl_batch_flash(
    state: State<'_, AppState>,
    rawprogram_path: String,
    patch_path: Option<String>,
    image_dir: String,
) -> Result<BatchFlashResult, String> {
    info!(
        "EDL: batch flash from {} (images: {})",
        rawprogram_path, image_dir
    );

    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;

    let conn = state
        .edl_connection
        .lock()
        .map_err(|e| format!("State lock failed: {e}"))?
        .take()
        .ok_or_else(|| "EDL not connected. Upload a programmer first.".to_string())?;

    let rp_path = PathBuf::from(&rawprogram_path);
    let p_path = patch_path.map(PathBuf::from);
    let img_dir = PathBuf::from(&image_dir);

    let result = tokio::task::spawn_blocking(move || {
        let mut conn = conn;
        let r = EdlProtocol::batch_flash(
            &mut conn,
            &rp_path,
            p_path.as_deref(),
            &img_dir,
        );
        (conn, r)
    })
    .await;

    match result {
        Ok((conn, r)) => {
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = Some(conn);
            r.map_err(|e| e.to_string())
        }
        Err(e) => {
            warn!("EDL: batch_flash panicked, connection lost: {e}");
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = None;
            Err(format!("EDL operation failed (internal error). Reconnect required. {e}"))
        }
    }
}

#[tauri::command]
pub async fn edl_validate_batch(
    rawprogram_path: String,
    image_dir: String,
) -> Result<Vec<String>, String> {
    let xml_path = std::path::PathBuf::from(&rawprogram_path);
    let dir = std::path::PathBuf::from(&image_dir);
    crate::protocols::edl::validate_batch_paths(&xml_path, &dir)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn edl_discover_rawprograms(
    dir_path: String,
) -> Result<Vec<RawprogramDiscovery>, String> {
    let dir = PathBuf::from(&dir_path);
    let sets = crate::protocols::edl_xml::discover_rawprograms(&dir)
        .map_err(|e| e.to_string())?;

    Ok(sets
        .into_iter()
        .map(|s| RawprogramDiscovery {
            rawprogram_path: s.rawprogram_path.to_string_lossy().into_owned(),
            patch_path: s.patch_path.map(|p| p.to_string_lossy().into_owned()),
            lun_hint: s.lun_hint,
        })
        .collect())
}

#[tauri::command]
pub async fn edl_batch_flash_dir(
    state: State<'_, AppState>,
    dir_path: String,
) -> Result<BatchFlashResult, String> {
    info!("EDL: batch flash directory {dir_path}");

    let _guard = FlashGuard::try_acquire(&state.is_flashing)?;

    let conn = state
        .edl_connection
        .lock()
        .map_err(|e| format!("State lock failed: {e}"))?
        .take()
        .ok_or_else(|| "EDL not connected. Upload a programmer first.".to_string())?;

    let dir = PathBuf::from(&dir_path);

    let result = tokio::task::spawn_blocking(move || {
        let mut conn = conn;
        let r = EdlProtocol::batch_flash_dir(&mut conn, &dir);
        (conn, r)
    })
    .await;

    match result {
        Ok((conn, r)) => {
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = Some(conn);
            r.map_err(|e| e.to_string())
        }
        Err(e) => {
            warn!("EDL: batch_flash_dir panicked, connection lost: {e}");
            *state.edl_connection.lock().map_err(|e| format!("State lock failed: {e}"))? = None;
            Err(format!("EDL operation failed (internal error). Reconnect required. {e}"))
        }
    }
}

// --- Programmer database commands ---

fn ensure_db_loaded(state: &AppState, app_handle: &AppHandle) -> Result<(), String> {
    let mut db = state.programmer_db.lock().map_err(|e| format!("DB lock failed: {e}"))?;
    if !db.is_loaded() {
        let app_dir = app_handle.path().app_data_dir()
            .map_err(|e| format!("Cannot get app data dir: {e}"))?;
        *db = ProgrammerDatabase::load(&app_dir);
    }
    Ok(())
}

#[tauri::command]
pub async fn edl_db_lookup(
    state: State<'_, AppState>,
    app_handle: AppHandle,
    hwid: String,
    pkhash: String,
) -> Result<Option<ProgrammerEntry>, String> {
    ensure_db_loaded(&state, &app_handle)?;
    let db = state.programmer_db.lock().map_err(|e| format!("DB lock failed: {e}"))?;
    let key = ProgrammerDatabase::make_key(&hwid, &pkhash);
    Ok(db.lookup_by_key(&key))
}

#[tauri::command]
pub async fn edl_db_list(
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<Vec<(String, ProgrammerEntry)>, String> {
    ensure_db_loaded(&state, &app_handle)?;
    let db = state.programmer_db.lock().map_err(|e| format!("DB lock failed: {e}"))?;
    Ok(db.list())
}

#[tauri::command]
pub async fn edl_db_remove(
    state: State<'_, AppState>,
    app_handle: AppHandle,
    key: String,
) -> Result<(), String> {
    ensure_db_loaded(&state, &app_handle)?;
    let mut db = state.programmer_db.lock().map_err(|e| format!("DB lock failed: {e}"))?;
    db.remove(&key);
    db.save().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn edl_scan_programmers(
    state: State<'_, AppState>,
    app_handle: AppHandle,
    dir_path: String,
    hwid: Option<String>,
    pkhash: Option<String>,
) -> Result<Vec<ProgrammerCandidate>, String> {
    let path = PathBuf::from(&dir_path);

    // Drain the identity cache so it can be moved into the blocking task.
    let mut cache = state.programmer_identity_cache
        .lock()
        .map_err(|e| format!("Identity cache lock failed: {e}"))?
        .drain()
        .collect::<HashMap<_, _>>();

    let (scan_result, updated_cache) = tokio::task::spawn_blocking(move || {
        let res = scan_programmers(&path, Some(&mut cache));
        (res, cache)
    })
    .await
    .map_err(|e| format!("Scan task failed: {e}"))?;

    let mut results = scan_result.map_err(|e| e.to_string())?;

    // Put the updated cache back.
    if let Ok(mut guard) = state.programmer_identity_cache.lock() {
        guard.extend(updated_cache);
    }

    // Score candidates if device identity is available
    if let (Some(hw), Some(pk)) = (hwid, pkhash) {
        ensure_db_loaded(&state, &app_handle)?;
        let db = state.programmer_db.lock().map_err(|e| format!("DB lock failed: {e}"))?;
        score_candidates(&mut results, &db, &hw, &pk);
    }

    Ok(results)
}

// --- Logcat commands ---

#[tauri::command]
pub async fn logcat_start(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    on_output: Channel<ShellOutput>,
    level: Option<String>,
    tag: Option<String>,
    pid: Option<u32>,
) -> Result<(), String> {
    let serial = {
        let dev = state.current_device.lock().map_err(|e| format!("Lock failed: {e}"))?;
        dev.as_ref().ok_or("No device connected")?.serial.clone()
            .ok_or("Device has no serial")?
    };
    let force_usb = *state.force_usb.lock().map_err(|e| format!("Lock failed: {e}"))?;

    info!("Logcat start: session={}, level={:?}, tag={:?}, pid={:?}", session_id, level, tag, pid);

    let serial_clone = serial.clone();
    let (reader, writer, _is_v2) = tokio::task::spawn_blocking(move || {
        adb_shell_open_command(&serial_clone, "logcat -B", force_usb)
    })
    .await
    .map_err(|e| format!("Logcat open failed: {e}"))?
    .map_err(|e| e.to_string())?;

    let writer = Arc::new(Mutex::new(writer));
    {
        let mut sessions = state.shell_sessions.lock()
            .map_err(|e| format!("Session lock failed: {e}"))?;
        if let Some(old) = sessions.remove(&session_id) {
            let _ = old.writer.lock().map(|mut s| s.shutdown());
        }
        sessions.insert(session_id.clone(), ShellSession { writer, is_v2: true });
    }

    let min_priority = level
        .and_then(|l| LogPriority::from_str(&l))
        .unwrap_or(LogPriority::Verbose);
    let filter = LogcatFilter { min_priority, tag, pid };

    let sid = session_id.clone();
    let app_clone = app.clone();
    tokio::task::spawn_blocking(move || {
        let mut reader = reader;
        let mut parser = LogcatParser::new();

        loop {
            match shell_v2_read_packet(&mut reader) {
                Ok((ShellV2Id::Stdout, data)) => {
                    let entries = parser.feed(&data);
                    for entry in entries {
                        if filter.matches(&entry) {
                            let line = entry.to_ansi();
                            if on_output.send(ShellOutput::Data {
                                data: format!("{line}\r\n").into_bytes(),
                            }).is_err() {
                                return;
                            }
                        }
                    }
                }
                Ok((ShellV2Id::Exit, _)) => {
                    let _ = on_output.send(ShellOutput::Exit {
                        message: "Logcat stream ended".into(),
                        code: None,
                    });
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    let msg = format!("Logcat connection lost: {e}");
                    let _ = on_output.send(ShellOutput::Exit { message: msg, code: None });
                    break;
                }
            }
        }

        if let Some(app_state) = app_clone.try_state::<AppState>() {
            let _ = app_state.shell_sessions.lock().map(|mut s| { s.remove(&sid); });
        }
        info!("Logcat reader exited: session={}", sid);
    });

    Ok(())
}

#[tauri::command]
pub async fn logcat_stop(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    info!("Logcat stop: session={}", session_id);

    let session_writer = {
        let sessions = state.shell_sessions.lock()
            .map_err(|e| format!("Session lock failed: {e}"))?;
        sessions.get(&session_id).map(|s| (s.writer.clone(), s.is_v2))
    };

    if let Some((writer, _is_v2)) = session_writer {
        if let Ok(mut w) = writer.lock() {
            use std::io::Write;
            let packet = shell_v2_build_packet(ShellV2Id::Stdin, &[0x03]);
            let _ = w.write_all(&packet);
        }
    }

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let mut sessions = state.shell_sessions.lock()
        .map_err(|e| format!("Session lock failed: {e}"))?;
    if let Some(session) = sessions.remove(&session_id) {
        let _ = session.writer.lock().map(|mut s| s.shutdown());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flash_guard_acquire_and_drop() {
        let flag = Mutex::new(false);
        {
            let _guard = FlashGuard::try_acquire(&flag).unwrap();
            assert!(*flag.lock().unwrap());
        }
        assert!(!*flag.lock().unwrap());
    }

    #[test]
    fn test_flash_guard_rejects_concurrent() {
        let flag = Mutex::new(false);
        let _guard = FlashGuard::try_acquire(&flag).unwrap();
        assert!(FlashGuard::try_acquire(&flag).is_err());
    }

    #[test]
    fn test_flash_guard_resets_on_panic_path() {
        let flag = Mutex::new(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = FlashGuard::try_acquire(&flag).unwrap();
            panic!("simulated failure");
        }));
        assert!(result.is_err());
        assert!(!*flag.lock().unwrap());
    }

    #[test]
    fn test_check_dump_resume_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let partitions = vec![("boot".to_string(), Some(1024))];
        let result = check_dump_resume(dir.path().to_string_lossy().to_string(), partitions);
        assert_eq!(result, vec!["boot"]);
    }

    #[test]
    fn test_check_dump_resume_matching_size_skips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boot.img");
        std::fs::write(&path, vec![0u8; 1024]).unwrap();
        let partitions = vec![("boot".to_string(), Some(1024))];
        let result = check_dump_resume(dir.path().to_string_lossy().to_string(), partitions);
        assert!(result.is_empty());
    }

    #[test]
    fn test_check_dump_resume_mismatch_size_returns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boot.img");
        std::fs::write(&path, vec![0u8; 512]).unwrap();
        let partitions = vec![("boot".to_string(), Some(1024))];
        let result = check_dump_resume(dir.path().to_string_lossy().to_string(), partitions);
        assert_eq!(result, vec!["boot"]);
    }

    #[test]
    fn test_check_dump_resume_unknown_size_skips_if_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boot.img");
        std::fs::write(&path, b"data").unwrap();
        let partitions = vec![("boot".to_string(), None)];
        let result = check_dump_resume(dir.path().to_string_lossy().to_string(), partitions);
        assert!(result.is_empty());
    }

    #[test]
    fn test_check_dump_resume_path_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let partitions = vec![("../etc/passwd".to_string(), Some(1024))];
        let result = check_dump_resume(dir.path().to_string_lossy().to_string(), partitions);
        assert_eq!(result, vec!["../etc/passwd"]);
    }
}
