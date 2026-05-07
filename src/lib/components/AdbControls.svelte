<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { open } from "@tauri-apps/plugin-dialog";
  import { currentDevice, deviceVars, devicePartitions, deviceReady, deviceHealth } from "../stores/device";
  import { logStore } from "../stores/log";
  import { flashStage, flashPercent, ensureFlashListener } from "../stores/flash";

  let sideloadPath = $state("");
  let isSideloading = $state(false);

  async function pickZip() {
    const selected = await open({
      multiple: false,
      filters: [
        { name: "OTA / ROM ZIP", extensions: ["zip"] },
        { name: "All files", extensions: ["*"] },
      ],
    });

    if (selected) {
      sideloadPath = selected as string;
      logStore.add(`Selected sideload file: ${sideloadPath}`);
    }
  }

  async function rebootToSideload() {
    try {
      logStore.add("Rebooting to recovery for sideload...");
      await invoke<void>("reboot_device", { mode: "Recovery" });
      logStore.add("Reboot to recovery sent. Enable sideload on device, then click Detect.");
      // Clear stale state — device is rebooting to recovery mode
      $currentDevice = null;
      $deviceVars = {};
      $devicePartitions = [];
      $deviceHealth = null;
      $deviceReady = false;
    } catch (err) {
      logStore.add(`Reboot failed: ${err}`, "error");
    }
  }

  async function startSideload() {
    if (!sideloadPath) return;
    isSideloading = true;
    ensureFlashListener();
    flashStage.set("Sending");
    flashPercent.set(0);
    logStore.add(`Sideload: ${sideloadPath}`);

    try {
      await invoke<void>("sideload_firmware", { firmwarePath: sideloadPath });
      logStore.add("Sideload completed successfully!");
    } catch (err) {
      logStore.add(`Sideload failed: ${err}`, "error");
      flashStage.set("Error");
    } finally {
      isSideloading = false;
    }
  }

  let canSideload = $derived(!!sideloadPath && !!$currentDevice && !isSideloading);
</script>

<div class="adb-controls">
  <div class="field">
    <div class="field-label">Sideload file (ZIP)</div>
    <div class="file-row">
      <input
        type="text"
        readonly
        value={sideloadPath || "No file selected"}
        class="file-input"
        class:placeholder={!sideloadPath}
      />
      <button class="btn-secondary" onclick={pickZip} disabled={isSideloading}>
        Browse
      </button>
    </div>
  </div>

  <div class="action-row">
    <button class="btn-primary" onclick={startSideload} disabled={!canSideload}>
      {isSideloading ? "Sideloading..." : "Sideload"}
    </button>
    <button class="btn-secondary" onclick={rebootToSideload} disabled={!$currentDevice}>
      Reboot to Recovery
    </button>
  </div>

  <div class="adb-note">
    ADB sideload transfers a ZIP to recovery mode. The device must be in recovery with sideload enabled.
  </div>
</div>

<style>
  .adb-controls {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .field-label {
    font-size: var(--font-base);
    font-weight: 500;
    color: var(--text-secondary);
  }

  .file-row {
    display: flex;
    gap: 8px;
  }

  .file-input {
    flex: 1;
    background: var(--input-bg);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 9px 12px;
    font-size: var(--font-base);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text);
  }

  .file-input.placeholder {
    color: var(--text-muted);
  }

  .action-row {
    display: flex;
    gap: 8px;
    margin-top: 4px;
  }

  .btn-primary {
    background: var(--primary);
    color: white;
    border: none;
    border-radius: 6px;
    padding: 10px 28px;
    font-size: var(--font-md);
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s;
  }

  .btn-primary:hover:not(:disabled) {
    background: var(--primary-hover);
  }

  .btn-primary:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .btn-secondary {
    background: var(--button-secondary);
    color: var(--text-secondary);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 10px 20px;
    font-size: var(--font-base);
    font-weight: 500;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
  }

  .btn-secondary:hover:not(:disabled) {
    background: var(--surface-hover);
    color: var(--text);
  }

  .btn-secondary:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .adb-note {
    font-size: var(--font-xs);
    color: var(--text-muted);
    line-height: 1.5;
  }
</style>
