<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { open } from "@tauri-apps/plugin-dialog";
  import { currentDevice, devicePartitions, deviceVars, deviceReady, deviceHealth } from "../stores/device";
  import {
    firmwarePath,
    selectedPartition,
    isFlashing,
    flashStage,
    flashMessage,
    ensureFlashListener,
  } from "../stores/flash";
  import { logStore } from "../stores/log";

  async function rebootDevice() {
    try {
      logStore.add("Rebooting device...");
      await invoke<void>("reboot_device", { mode: "Normal" });
      logStore.add("Reboot sent — click Detect when device restarts");
      // Clear stale state — device is rebooting to a different mode
      $currentDevice = null;
      $deviceVars = {};
      $devicePartitions = [];
      $deviceHealth = null;
      $deviceReady = false;
    } catch (err) {
      logStore.add(`Reboot failed: ${err}`, "error");
    }
  }

  const defaultPartitions = [
    "boot",
    "recovery",
    "system",
    "vendor",
    "dtbo",
    "vbmeta",
    "super",
    "userdata",
  ];

  let partitions = $derived($devicePartitions.length > 0 ? $devicePartitions : defaultPartitions);

  async function pickFile() {
    const selected = await open({
      multiple: false,
      filters: [
        { name: "Firmware", extensions: ["img", "bin", "mbn", "elf", "raw"] },
        { name: "All files", extensions: ["*"] },
      ],
    });

    if (selected) {
      $firmwarePath = selected as string;
      logStore.add(`Selected firmware: ${$firmwarePath}`);
    }
  }

  async function startFlash() {
    if (!$firmwarePath || !$currentDevice) return;

    try {
      const isCritical = await invoke<boolean>("check_critical_partition", {
        partition: $selectedPartition,
      });
      if (isCritical) {
        const confirmed = confirm(
          `WARNING: '${$selectedPartition}' is a critical partition.\n\n` +
            `Flashing this partition incorrectly can BRICK your device.\n\n` +
            `Are you sure you want to continue?`,
        );
        if (!confirmed) return;
      }
    } catch (err) {
      logStore.add(`Error checking partition: ${err}`, "error");
    }

    $isFlashing = true;
    $flashStage = "Validating";
    $flashMessage = "Starting flash...";
    ensureFlashListener();
    logStore.add(
      `Flashing ${$firmwarePath} → ${$selectedPartition} (${$currentDevice.protocol})`,
    );

    try {
      await invoke<void>("flash_firmware", {
        firmwarePath: $firmwarePath,
        partition: $selectedPartition,
      });
      logStore.add("Flash completed successfully!");
    } catch (err) {
      logStore.add(`Flash failed: ${err}`, "error");
      $flashStage = "Error";
      $flashMessage = `${err}`;
    } finally {
      $isFlashing = false;
    }
  }

  let canFlash = $derived($firmwarePath && $currentDevice && !$isFlashing);

  let dropdownOpen = $state(false);

  function selectPartition(p: string) {
    $selectedPartition = p;
    dropdownOpen = false;
  }

  function handleClickOutside(event: MouseEvent) {
    const target = event.target as HTMLElement;
    if (!target.closest(".dropdown")) {
      dropdownOpen = false;
    }
  }
</script>

<svelte:window onclick={handleClickOutside} />

<div class="flash-controls">
  <div class="field">
    <div class="field-label">Firmware file</div>
    <div class="file-row">
      <input
        type="text"
        readonly
        value={$firmwarePath || "No file selected"}
        class="file-input"
        class:placeholder={!$firmwarePath}
      />
      <button class="btn-secondary" onclick={pickFile} disabled={$isFlashing}>
        Browse
      </button>
    </div>
  </div>

  <div class="field">
    <div class="field-label">Target partition</div>
    <div class="dropdown" class:open={dropdownOpen}>
      <button
        class="dropdown-trigger"
        onclick={() => { if (!$isFlashing) dropdownOpen = !dropdownOpen; }}
        disabled={$isFlashing}
      >
        <span>{$selectedPartition}</span>
        <span class="dropdown-arrow">{dropdownOpen ? "\u25B2" : "\u25BC"}</span>
      </button>
      {#if dropdownOpen}
        <div class="dropdown-menu">
          {#each partitions as p}
            <button
              class="dropdown-item"
              class:selected={p === $selectedPartition}
              onclick={() => selectPartition(p)}
            >
              {p}
            </button>
          {/each}
        </div>
      {/if}
    </div>
  </div>

  <div class="action-row">
    <button class="btn-primary" onclick={startFlash} disabled={!canFlash}>
      {$isFlashing ? "Flashing..." : "Flash"}
    </button>
    <button class="btn-secondary" onclick={rebootDevice} disabled={!$currentDevice || $isFlashing}>
      Reboot
    </button>
  </div>
</div>

<style>
  .flash-controls {
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

  .dropdown {
    position: relative;
    width: 180px;
  }

  .dropdown-trigger {
    width: 100%;
    display: flex;
    justify-content: space-between;
    align-items: center;
    background: var(--input-bg);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 9px 12px;
    font-size: var(--font-base);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text);
    cursor: pointer;
    transition: border-color 0.15s;
  }

  .dropdown-trigger:hover:not(:disabled) {
    border-color: var(--primary);
  }

  .dropdown-trigger:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .dropdown-arrow {
    font-size: var(--font-xs);
    color: var(--text-muted);
  }

  .dropdown-menu {
    position: absolute;
    top: calc(100% + 4px);
    left: 0;
    width: 100%;
    background: var(--surface);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 4px 0;
    z-index: 10;
    max-height: 200px;
    overflow-y: auto;
  }

  .dropdown-item {
    display: block;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    padding: 8px 12px;
    font-size: var(--font-base);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text-secondary);
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
  }

  .dropdown-item:hover {
    background: var(--surface-hover);
    color: var(--text);
  }

  .dropdown-item.selected {
    color: var(--primary);
    font-weight: 600;
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
</style>
