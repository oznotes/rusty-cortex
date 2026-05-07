<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { currentDevice, isDetecting, deviceVars, devicePartitions, deviceReady, deviceHealth, edlInfo, edlConnected, edlPartitions } from "../stores/device";
  import { usbDirectMode } from "../stores/usb";
  import { logStore } from "../stores/log";
  import { flashStage, flashMessage, flashPercent } from "../stores/flash";
  import type { DeviceInfo, DeviceHealth, EdlDeviceInfo, ProtocolType, RebootMode } from "../types";

  async function detectDevice() {
    $isDetecting = true;
    $deviceReady = false;
    // Reset all progress/error state from previous operations
    flashStage.set("Idle");
    flashMessage.set("");
    flashPercent.set(null);
    // Reset EDL stores
    $edlInfo = null;
    $edlConnected = false;
    $edlPartitions = [];
    logStore.add("Scanning for devices...");

    try {
      const device = await invoke<DeviceInfo | null>("detect_device");
      $currentDevice = device;

      if (device) {
        const inRecovery = device.adb_state === "Recovery" || device.adb_state === "Sideload";
        logStore.add(
          `Found: ${device.product ?? "Unknown"} (${device.protocol}${inRecovery ? " — " + device.adb_state : ""}) — ${device.serial ?? "no serial"}`
        );
        if (device.protocol === "Edl") {
          await queryEdlIdentity();
        } else if (!inRecovery) {
          await queryDeviceInfo(device.protocol);
          if (device.protocol === "Adb") {
            queryDeviceHealth();
          }
        } else {
          logStore.add(`Device in ${device.adb_state} mode — properties unavailable`);
          $deviceVars = {};
          $devicePartitions = [];
          $deviceHealth = null;
        }
        $deviceReady = true;
      } else {
        logStore.add("No device found", "warn");
        $deviceVars = {};
        $devicePartitions = [];
        $deviceHealth = null;
        $deviceReady = false;
      }
    } catch (err) {
      logStore.add(`Detection failed: ${err}`, "error");
    } finally {
      $isDetecting = false;
    }
  }

  async function queryDeviceInfo(protocol: ProtocolType) {
    try {
      logStore.add("Querying device info...");
      const vars = await invoke<Record<string, string>>("get_device_vars", { protocol });
      $deviceVars = vars;

      if (protocol === "Adb") {
        // ADB properties use Android getprop keys
        const summary = [
          vars["ro.product.model"],
          vars["ro.product.device"],
          vars["ro.build.version.release"] ? `Android ${vars["ro.build.version.release"]}` : null,
          vars["ro.build.display.id"],
        ].filter(Boolean).join(" | ");
        logStore.add(summary);
        logStore.add(`${Object.keys(vars).length} properties queried`);
      } else {
        // Fastboot variables
        const summary = [
          vars["product"] || vars["model"],
          vars["variant"],
          vars["secure"] === "yes" ? "secure" : "insecure",
          vars["unlocked"] === "yes" ? "unlocked" : "locked",
          vars["battery-voltage"] ? `battery ${vars["battery-voltage"]}mV` : null,
        ].filter(Boolean).join(" | ");
        logStore.add(summary);
        logStore.add(`${Object.keys(vars).length} variables queried`);
      }

      const partitions = await invoke<string[]>("get_partitions", { protocol });
      $devicePartitions = partitions;
      if (partitions.length > 0) {
        logStore.add(`${partitions.length} partitions found`);
      }
    } catch (err) {
      logStore.add(`Device query failed: ${err}`, "warn");
    }
  }

  async function rebootTo(mode: RebootMode) {
    try {
      logStore.add(`Rebooting to ${mode}...`);
      await invoke<void>("reboot_device", { mode });
      logStore.add(`Reboot to ${mode} sent — detecting new mode...`);

      // Clear stale state immediately — device is changing modes
      $currentDevice = null;
      $deviceVars = {};
      $devicePartitions = [];
      $deviceHealth = null;
      $deviceReady = false;
      flashStage.set("Idle");
      flashMessage.set("");
      flashPercent.set(null);

      // Two-phase auto-detect (same pattern as `adb wait-for-device`):
      // Phase 1: wait for device to disconnect from USB
      // Phase 2: wait for device to reappear in new mode
      $isDetecting = true;
      try {
        // Phase 1: Wait for disconnect (1s intervals, up to 5s)
        for (let i = 0; i < 5; i++) {
          await new Promise(r => setTimeout(r, 1000));
          try {
            const d = await invoke<DeviceInfo | null>("detect_device");
            if (!d) break; // device disconnected
          } catch { break; }
        }

        // Phase 2: Wait for reconnect in new mode (2s intervals, up to 5 attempts)
        // Only sets device presence — full query is Detect button's job.
        for (let attempt = 1; attempt <= 5; attempt++) {
          await new Promise(r => setTimeout(r, 2000));
          try {
            const device = await invoke<DeviceInfo | null>("detect_device");
            if (device) {
              $currentDevice = device;
              logStore.add(`Device found in ${device.protocol} mode — click Detect for full info`);
              return;
            }
          } catch { /* device still booting */ }
        }
        logStore.add("Device not found after reboot — click Detect when ready", "warn");
      } finally {
        $isDetecting = false;
      }
    } catch (err) {
      logStore.add(`Reboot failed: ${err}`, "error");
    }
  }

  async function queryDeviceHealth() {
    try {
      const health = await invoke<DeviceHealth>("get_device_health");
      $deviceHealth = health;
      const parts = [];
      if (health.battery_level !== null) parts.push(`Battery ${health.battery_level}%`);
      if (health.ram_total_gb !== null) parts.push(`RAM ${health.ram_total_gb.toFixed(1)} GB`);
      if (parts.length > 0) logStore.add(`Health: ${parts.join(" | ")}`);
    } catch (err) {
      logStore.add(`Health check failed: ${err}`, "warn");
      $deviceHealth = null;
    }
  }

  async function queryEdlIdentity() {
    try {
      logStore.add("Querying EDL device identity (Sahara)...");
      const info = await invoke<EdlDeviceInfo>("edl_identify");
      $edlInfo = info;
      const parts = [
        info.serial ? `Serial ${info.serial}` : null,
        info.hw_id ? `HWID ${info.hw_id}` : null,
      ].filter(Boolean).join(" | ");
      logStore.add(`EDL: ${parts || "identity queried"}`);
    } catch (err) {
      logStore.add(`EDL identify failed: ${err}`, "warn");
    }
  }

  async function edlRebootFromSidebar(mode: string) {
    try {
      logStore.add(`EDL reboot: ${mode}`);
      await invoke<void>("edl_reboot", { mode });
      logStore.add(`Reboot (${mode}) command sent`);
      if (mode === "reset" || mode === "off") {
        logStore.add("If device doesn't respond, power cycle manually");
      }
      // Clear EDL state
      try { await invoke<void>("edl_disconnect"); } catch { /* best effort */ }
      $edlConnected = false;
      $edlPartitions = [];
      $edlInfo = null;

      // Clear device state
      $currentDevice = null;
      $deviceVars = {};
      $devicePartitions = [];
      $deviceHealth = null;
      $deviceReady = false;

      // Post-reboot detection
      await new Promise(r => setTimeout(r, 1500));
      try {
        const device = await invoke<DeviceInfo | null>("detect_device");
        if (device) {
          $currentDevice = device;
          logStore.add(`Device found in ${device.protocol} mode — click Detect for full info`);
        } else {
          logStore.add("Device disconnected after reboot");
        }
      } catch { /* device gone */ }
    } catch (err) {
      logStore.add(`EDL reboot failed: ${err}`, "error");
    }
  }

  let hasDevice = $derived(!!$currentDevice);
</script>

<aside class="sidebar">
  <!-- Device Section -->
  <div class="section">
    <div class="section-header">Device</div>
    {#if $currentDevice}
      {#if $currentDevice.protocol === "Adb"}
        {@const inRecovery = $currentDevice.adb_state === "Recovery" || $currentDevice.adb_state === "Sideload"}
        <div class="device-name">{inRecovery ? ($currentDevice.product || "Android Device") : ($deviceVars["ro.product.model"] || $currentDevice.product || "Unknown Device")}</div>
        {#if !inRecovery && $deviceVars["ro.product.device"]}
          <div class="device-variant">{$deviceVars["ro.product.device"]}</div>
        {/if}
        <div class="device-serial">{$currentDevice.serial ?? "No serial"}</div>
        {#if inRecovery}
          <div class="device-status">
            <span class="status-tag recovery-tag">{$currentDevice.adb_state} mode</span>
          </div>
        {:else if $deviceVars["ro.build.version.release"]}
          <div class="device-status">
            <span class="status-tag">Android {$deviceVars["ro.build.version.release"]}</span>
          </div>
        {/if}
      {:else if $currentDevice.protocol === "Edl"}
        <div class="device-name">Qualcomm EDL Device</div>
        {#if $edlInfo?.serial}
          <div class="device-serial">{$edlInfo.serial}</div>
        {:else}
          <div class="device-serial">{$currentDevice.serial ?? "No serial"}</div>
        {/if}
        {#if $edlInfo?.chipset}
          <div class="device-variant">{$edlInfo.chipset}</div>
        {/if}
        {#if $edlInfo?.hw_id}
          <div class="device-status">
            <span class="status-tag" title={$edlInfo.hw_id}>HWID {$edlInfo.hw_id.slice(0, 16)}...</span>
          </div>
        {/if}
      {:else}
        <div class="device-name">{$deviceVars["product"] || $deviceVars["model"] || $currentDevice.product || "Unknown Device"}</div>
        {#if $deviceVars["variant"]}
          <div class="device-variant">{$deviceVars["variant"]}</div>
        {/if}
        <div class="device-serial">{$currentDevice.serial ?? "No serial"}</div>
        {#if $deviceVars["secure"] || $deviceVars["unlocked"]}
          <div class="device-status">
            {#if $deviceVars["secure"] === "yes"}
              <span class="status-tag">secure</span>
            {/if}
            {#if $deviceVars["unlocked"] === "yes"}
              <span class="status-tag unlocked">unlocked</span>
            {:else if $deviceVars["unlocked"] === "no"}
              <span class="status-tag locked">locked</span>
            {/if}
          </div>
        {/if}
      {/if}
    {:else}
      <div class="device-name empty">No device</div>
      <div class="device-serial">Connect via USB</div>
    {/if}
  </div>

  <!-- Mode Section -->
  <div class="section">
    <div class="section-header">Mode</div>
    {#if $currentDevice}
      {@const inRecovery = $currentDevice.adb_state === "Recovery" || $currentDevice.adb_state === "Sideload"}
      <div class="mode-badge" class:recovery={inRecovery}>
        <span class="mode-dot" class:recovery-dot={inRecovery}></span>
        <span class="mode-text" class:recovery-text={inRecovery}>
          {#if inRecovery}
            {$currentDevice.adb_state}
          {:else}
            {$currentDevice.protocol}
          {/if}
        </span>
      </div>
    {:else}
      <div class="mode-badge disconnected">
        <span class="mode-dot off"></span>
        <span class="mode-text">—</span>
      </div>
    {/if}
  </div>

  <!-- Health Section -->
  {#if $deviceHealth && $currentDevice?.protocol === "Adb"}
    <div class="section">
      <div class="section-header">Health</div>
      <div class="health-grid">
        {#if $deviceHealth.battery_level !== null}
          <span class="health-label">Battery</span>
          <span class="health-value">
            {$deviceHealth.battery_level}%
            {#if $deviceHealth.battery_health}
              <span class="health-status" class:health-good={$deviceHealth.battery_health === "Good"} class:health-warn={$deviceHealth.battery_health !== "Good"}>{$deviceHealth.battery_health}</span>
            {/if}
          </span>
        {/if}
        {#if $deviceHealth.battery_temp !== null}
          <span class="health-label">Temp</span>
          <span class="health-value">{$deviceHealth.battery_temp.toFixed(1)}&deg;C</span>
        {/if}
        {#if $deviceHealth.storage_total_gb !== null}
          <span class="health-label">Storage</span>
          <span class="health-value">{$deviceHealth.storage_used_gb?.toFixed(1)} / {$deviceHealth.storage_total_gb.toFixed(0)} GB</span>
        {/if}
        {#if $deviceHealth.ram_total_gb !== null}
          <span class="health-label">RAM</span>
          <span class="health-value">{$deviceHealth.ram_used_gb?.toFixed(1)} / {$deviceHealth.ram_total_gb.toFixed(1)} GB</span>
        {/if}
        {#if $deviceVars["ro.boot.flash.locked"]}
          <span class="health-label">Boot</span>
          <span class="health-value">
            {#if $deviceVars["ro.boot.flash.locked"] === "1"}
              <span class="health-status health-locked">Locked</span>
            {:else}
              <span class="health-status health-good">Unlocked</span>
            {/if}
          </span>
        {/if}
      </div>
    </div>
  {/if}

  <!-- Reboot To Section -->
  <div class="section">
    <div class="section-header">Reboot to</div>
    {#if $currentDevice?.protocol === "Edl"}
      <div class="reboot-actions">
        <button class="reboot-btn" onclick={() => edlRebootFromSidebar("reset")} disabled={!$edlConnected}>Reboot</button>
        <button class="reboot-btn" onclick={() => edlRebootFromSidebar("reset_to_edl")} disabled={!$edlConnected}>EDL</button>
        <button class="reboot-btn warning" onclick={() => edlRebootFromSidebar("off")} disabled={!$edlConnected}>Power Off</button>
      </div>
      {#if !$edlConnected}
        <span class="hint-edl">Connect programmer first</span>
      {/if}
    {:else}
      <div class="reboot-actions">
        <button class="reboot-btn" onclick={() => rebootTo("Normal")} disabled={!hasDevice}>System</button>
        <button class="reboot-btn" onclick={() => rebootTo("Recovery")} disabled={!hasDevice}>Recovery</button>
        <button class="reboot-btn" onclick={() => rebootTo("Bootloader")} disabled={!hasDevice}>Bootloader</button>
        <button class="reboot-btn warning" onclick={() => rebootTo("Edl")} disabled={!hasDevice}>EDL Mode</button>
      </div>
    {/if}
  </div>

  <!-- Spacer -->
  <div class="spacer"></div>

  <!-- USB Direct Toggle -->
  <div class="usb-toggle">
    <span class="usb-label">USB Direct</span>
    <button
      class="toggle-track"
      class:active={$usbDirectMode}
      onclick={() => usbDirectMode.update(v => !v)}
      role="switch"
      aria-checked={$usbDirectMode}
      aria-label="USB Direct mode"
    >
      <span class="toggle-knob"></span>
    </button>
  </div>

  <!-- Detect Button -->
  <div class="detect-section">
    <button class="detect-btn" onclick={detectDevice} disabled={$isDetecting}>
      {$isDetecting ? "Scanning..." : "Detect Device"}
    </button>
  </div>
</aside>

<style>
  .sidebar {
    width: 200px;
    background: var(--surface);
    border-right: 1px solid var(--border);
    padding: 20px 16px;
    display: flex;
    flex-direction: column;
    flex-shrink: 0;
  }

  .section {
    margin-bottom: 24px;
  }

  .section-header {
    font-size: var(--font-sm);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-label);
    margin-bottom: 12px;
  }

  .device-name {
    font-size: var(--font-md);
    font-weight: 600;
    color: var(--text);
    line-height: 1.4;
  }

  .device-name.empty {
    color: var(--text-muted);
  }

  .device-variant {
    font-size: var(--font-sm);
    color: var(--text-secondary);
    margin-top: 4px;
  }

  .device-serial {
    font-size: var(--font-sm);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text-muted);
    margin-top: 4px;
  }

  .device-status {
    display: flex;
    gap: 4px;
    margin-top: 8px;
  }

  .status-tag {
    font-size: var(--font-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    padding: 2px 8px;
    border-radius: 6px;
    background: rgba(var(--primary-rgb), 0.1);
    color: var(--primary);
  }

  .status-tag.locked {
    background: rgba(var(--danger-rgb), 0.1);
    color: var(--danger);
  }

  .status-tag.unlocked {
    background: rgba(var(--success-rgb), 0.1);
    color: var(--success);
  }

  .mode-badge {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    background: rgba(var(--primary-rgb), 0.1);
    border: 1px solid rgba(var(--primary-rgb), 0.2);
    border-radius: 6px;
    padding: 6px 12px;
  }

  .mode-badge.disconnected {
    background: var(--input-bg);
    border-color: var(--border);
  }

  .mode-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--success);
    box-shadow: 0 0 6px rgba(var(--success-rgb), 0.4);
  }

  .mode-dot.off {
    background: var(--text-muted);
    box-shadow: none;
  }

  .mode-text {
    font-size: var(--font-base);
    font-weight: 600;
    color: var(--primary);
  }

  .mode-badge.disconnected .mode-text {
    color: var(--text-muted);
  }

  .health-grid {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 4px 12px;
    align-items: baseline;
  }

  .health-label {
    font-size: var(--font-sm);
    color: var(--text-muted);
  }

  .health-value {
    font-size: var(--font-sm);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text-secondary);
  }

  .health-status {
    font-size: var(--font-xs);
    font-weight: 600;
  }

  .health-status.health-good {
    color: var(--success);
  }

  .health-status.health-warn {
    color: var(--warning);
  }

  .health-status.health-locked {
    color: var(--danger);
  }

  .reboot-actions {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .reboot-btn {
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 7px 12px;
    font-size: var(--font-base);
    color: var(--text-secondary);
    cursor: pointer;
    text-align: left;
    transition: background 0.15s, color 0.15s;
  }

  .reboot-btn:hover:not(:disabled) {
    background: var(--surface-hover);
    color: var(--text);
  }

  .hint-edl {
    font-size: var(--font-xs);
    color: var(--text-muted);
  }

  .reboot-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .reboot-btn.warning {
    color: var(--warning);
    border-color: var(--border);
    background: var(--input-bg);
  }

  .spacer {
    flex: 1;
  }

  .detect-section {
    border-top: 1px solid var(--border);
    padding-top: 16px;
  }

  .detect-btn {
    width: 100%;
    background: var(--primary);
    border: none;
    border-radius: 6px;
    padding: 9px 16px;
    font-size: var(--font-base);
    font-weight: 600;
    color: white;
    cursor: pointer;
    transition: background 0.15s;
  }

  .detect-btn:hover:not(:disabled) {
    background: var(--primary-hover);
  }

  .detect-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .mode-badge.recovery {
    background: rgba(var(--warning-rgb), 0.1);
    border-color: rgba(var(--warning-rgb), 0.2);
  }

  .mode-dot.recovery-dot {
    background: var(--warning);
    box-shadow: 0 0 6px rgba(var(--warning-rgb), 0.4);
  }

  .mode-text.recovery-text {
    color: var(--warning);
  }

  .status-tag.recovery-tag {
    background: rgba(var(--warning-rgb), 0.1);
    color: var(--warning);
  }

  .usb-toggle {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 8px 16px;
  }

  .usb-label {
    font-size: 12px;
    font-weight: 500;
    color: var(--text-secondary);
  }

  .toggle-track {
    position: relative;
    width: 32px;
    height: 18px;
    border-radius: 6px;
    background: var(--input-bg);
    border: 1px solid var(--border);
    cursor: pointer;
    transition: background 0.15s, border-color 0.15s;
    padding: 0;
  }

  .toggle-track.active {
    background: var(--primary);
    border-color: var(--primary);
  }

  .toggle-knob {
    position: absolute;
    top: 1px;
    left: 1px;
    width: 14px;
    height: 14px;
    border-radius: 6px;
    background: white;
    transition: transform 0.15s;
  }

  .toggle-track.active .toggle-knob {
    transform: translateX(14px);
  }
</style>
