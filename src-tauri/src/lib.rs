mod commands;
mod device;
mod error;
mod flash;
mod protocols;
mod types;

pub use error::FlashError;
pub use types::*;

use commands::AppState;
use tracing_appender::rolling;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

fn init_logging() {
    let log_dir = dirs_next::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("rusty-cortex")
        .join("logs");

    std::fs::create_dir_all(&log_dir).ok();

    let file_appender = rolling::daily(&log_dir, "rusty-cortex.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(std::io::stdout))
        .with(fmt::layer().with_ansi(false).with_writer(non_blocking))
        .init();

    tracing::info!("Logging initialized. Log dir: {}", log_dir.display());

    // Leak the guard so the non-blocking writer stays alive for the app lifetime
    std::mem::forget(_guard);
}

pub fn run() {
    init_logging();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::detect_device,
            commands::get_partitions,
            commands::flash_firmware,
            commands::check_critical_partition,
            commands::get_device_vars,
            commands::reboot_device,
            commands::sideload_firmware,
            commands::push_file,
            commands::pull_file,
            commands::shell_open,
            commands::shell_write,
            commands::shell_close,
            commands::shell_resize,
            commands::check_root,
            commands::get_device_health,
            commands::list_partitions_dump,
            commands::dump_partitions,
            commands::dump_image,
            commands::write_partition,
            commands::check_files_exist,
            commands::check_dump_resume,
            commands::install_apk,
            commands::adb_local_command,
            commands::list_device_directory,
            commands::set_usb_mode,
            commands::get_usb_mode,
            commands::edl_identify,
            commands::edl_connect,
            commands::edl_list_partitions,
            commands::edl_read_partition,
            commands::edl_reboot,
            commands::edl_disconnect,
            commands::edl_program_partition,
            commands::edl_erase_partition,
            commands::edl_batch_flash,
            commands::edl_validate_batch,
            commands::edl_discover_rawprograms,
            commands::edl_batch_flash_dir,
            commands::edl_db_lookup,
            commands::edl_db_list,
            commands::edl_db_remove,
            commands::edl_scan_programmers,
            commands::logcat_start,
            commands::logcat_stop,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
