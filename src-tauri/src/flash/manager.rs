use std::path::Path;

use tauri::{AppHandle, Emitter};
use tracing::{error, info};

use crate::error::FlashError;
use crate::flash::validation;
use crate::protocols::fastboot::FastbootProtocol;
use crate::types::{FlashProgress, FlashStage};

fn emit_progress(app: &AppHandle, stage: FlashStage, message: &str, percent: Option<f32>) {
    let progress = FlashProgress {
        stage: stage.clone(),
        message: message.to_string(),
        percent,
    };
    if let Err(e) = app.emit("flash-progress", &progress) {
        tracing::warn!("Failed to emit flash progress: {e}");
    }
    // Only log milestones
    match stage {
        FlashStage::Complete | FlashStage::Error | FlashStage::Validating => info!("{}", message),
        _ => {
            if let Some(p) = percent {
                let p = p as u32;
                if p == 0 || p == 25 || p == 50 || p == 75 || p == 100 {
                    info!("{}", message);
                }
            } else {
                info!("{}", message);
            }
        }
    }
}

/// Run the full flash workflow: validate → send → flash.
pub async fn run_flash(
    app: &AppHandle,
    protocol: &FastbootProtocol,
    firmware: &Path,
    partition: &str,
) -> Result<(), FlashError> {
    emit_progress(app, FlashStage::Validating, "Validating firmware file...", None);
    validation::validate_firmware(firmware)?;
    validation::validate_partition(partition)?;

    if validation::is_critical_partition(partition) {
        emit_progress(
            app,
            FlashStage::Validating,
            &format!("WARNING: '{}' is a critical partition. Flashing may brick the device.", partition),
            None,
        );
    }

    let device = protocol.detect().await?;
    if device.is_none() {
        return Err(FlashError::NoDevice);
    }

    emit_progress(
        app,
        FlashStage::Sending,
        &format!("Flashing {} to '{}'...", firmware.display(), partition),
        Some(0.0),
    );

    let app_ref = app.clone();
    let progress_cb = move |percent: f32| {
        let msg = format!("Sending... {:.0}%", percent);
        emit_progress(&app_ref, FlashStage::Sending, &msg, Some(percent));
    };

    match protocol.flash_with_progress(firmware, partition, Some(&progress_cb)).await {
        Ok(()) => {
            emit_progress(app, FlashStage::Complete, "Flash complete!", None);
            Ok(())
        }
        Err(e) => {
            let msg = format!("Flash failed: {}", e);
            emit_progress(app, FlashStage::Error, &msg, None);
            error!("{}", msg);
            Err(e)
        }
    }
}
