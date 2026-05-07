<script lang="ts">
  import { currentDevice, edlInfo, edlConnected } from "../stores/device";
  import FlashControls from "./FlashControls.svelte";
  import AdbControls from "./AdbControls.svelte";
  import FileTransfer from "./FileTransfer.svelte";
  import DumpControls from "./DumpControls.svelte";
  import EdlControls from "./EdlControls.svelte";
  import ProgressBar from "./ProgressBar.svelte";

  let adbTab = $state<"sideload" | "transfer" | "dump">("sideload");

  let inRecovery = $derived(
    $currentDevice?.adb_state === "Recovery" || $currentDevice?.adb_state === "Sideload"
  );

  $effect(() => {
    if (inRecovery && adbTab === "dump") {
      adbTab = "sideload";
    }
  });
</script>

<div class="workspace">
  {#if $currentDevice}
    {#if $currentDevice.protocol === "Fastboot"}
      <div class="section-header">Flash Firmware</div>
      <FlashControls />
      <ProgressBar />
    {:else if $currentDevice.protocol === "Adb"}
      <div class="tab-bar">
        <button
          class="tab"
          class:active={adbTab === "sideload"}
          onclick={() => (adbTab = "sideload")}
        >
          Sideload
        </button>
        <button
          class="tab"
          class:active={adbTab === "transfer"}
          onclick={() => (adbTab = "transfer")}
        >
          File Transfer
        </button>
        <button
          class="tab"
          class:active={adbTab === "dump"}
          class:disabled-tab={inRecovery}
          onclick={() => { if (!inRecovery) adbTab = "dump"; }}
          title={inRecovery ? "Not available in recovery mode" : ""}
        >
          Partitions
        </button>
      </div>
      {#if adbTab === "sideload"}
        <AdbControls />
      {:else if adbTab === "transfer"}
        <FileTransfer />
      {:else}
        {#if inRecovery}
          <div class="recovery-notice">Partition operations are not available in recovery mode. Reboot to system first.</div>
        {:else}
          <DumpControls />
        {/if}
      {/if}
      <ProgressBar />
    {:else if $currentDevice.protocol === "Edl"}
      <div class="section-header edl-header">
        <span>EDL (Qualcomm)</span>
        {#if $edlConnected && $edlInfo}
          <div class="edl-badges">
            {#if $edlInfo.storage_type}
              <span class="edl-badge">{$edlInfo.storage_type.toUpperCase()}</span>
            {/if}
            {#if $edlInfo.sector_size}
              <span class="edl-badge">{$edlInfo.sector_size}B</span>
            {/if}
            {#if $edlInfo.num_luns}
              <span class="edl-badge">{$edlInfo.num_luns} LUN{$edlInfo.num_luns > 1 ? "s" : ""}</span>
            {/if}
          </div>
        {/if}
      </div>
      <EdlControls />
      <ProgressBar />
    {:else}
      <div class="empty-state">
        <div class="empty-text">Protocol not yet supported: {$currentDevice.protocol}</div>
      </div>
    {/if}
  {:else}
    <div class="empty-state">
      <div class="empty-text">Connect a device to begin</div>
    </div>
  {/if}
</div>

<style>
  .workspace {
    flex: 1;
    padding: 24px 28px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }

  .section-header {
    font-size: var(--font-sm);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-label);
    margin-bottom: 20px;
  }

  .edl-header {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .edl-badges {
    display: flex;
    gap: 4px;
  }

  .edl-badge {
    font-size: var(--font-xs);
    font-weight: 600;
    letter-spacing: 0.03em;
    padding: 2px 8px;
    border-radius: 6px;
    background: rgba(var(--primary-rgb), 0.1);
    color: var(--primary);
  }

  .tab-bar {
    display: flex;
    gap: 0;
    margin-bottom: 20px;
    border-bottom: 1px solid var(--border);
  }

  .tab {
    font-size: var(--font-sm);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-label);
    background: none;
    border: none;
    border-bottom: 2px solid transparent;
    padding: 8px 16px;
    cursor: pointer;
    transition: color 0.15s, border-color 0.15s;
  }

  .tab:hover {
    color: var(--text-secondary);
  }

  .tab.active {
    color: var(--primary);
    border-bottom-color: var(--primary);
  }

  .empty-state {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .empty-text {
    font-size: var(--font-md);
    color: var(--text-muted);
  }

  .tab.disabled-tab {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .tab.disabled-tab:hover {
    color: var(--text-label);
  }

  .recovery-notice {
    font-size: var(--font-base);
    color: var(--text-muted);
    padding: 24px 0;
  }
</style>
