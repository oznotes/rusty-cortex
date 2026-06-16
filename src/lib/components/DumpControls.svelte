<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { currentDevice } from "../stores/device";
  import { logStore } from "../stores/log";
  import type { RootStatus, PartitionInfo, DumpListResult } from "../types";
  import PartitionRead from "./PartitionRead.svelte";
  import PartitionWrite from "./PartitionWrite.svelte";

  let rootStatus = $state<RootStatus | null>(null);
  let partitions = $state<PartitionInfo[]>([]);
  let tempDir = $state("");
  let freeBytes = $state<number | null>(null);
  let supportsShellV2 = $state(false);
  let activeTab = $state<"read" | "write">("read");
  let isReading = $state(false);
  let isWriting = $state(false);

  let hasRoot = $derived(
    rootStatus !== null && rootStatus.root_type !== "None"
  );

  $effect(() => {
    if ($currentDevice?.protocol === "Adb") {
      checkRoot();
    } else {
      rootStatus = null;
      partitions = [];
    }
  });

  async function checkRoot() {
    try {
      rootStatus = await invoke<RootStatus>("check_root");
      logStore.add(`Root check: ${rootStatus.message}`);
      if (rootStatus.root_type !== "None") {
        loadPartitions();
      }
    } catch (err) {
      logStore.add(`Root check failed: ${err}`, "error");
      rootStatus = { root_type: "None", message: `Check failed: ${err}` };
    }
  }

  async function loadPartitions() {
    try {
      const result = await invoke<DumpListResult>("list_partitions_dump");
      partitions = result.partitions;
      tempDir = result.temp_dir;
      freeBytes = result.free_bytes;
      supportsShellV2 = result.supports_shell_v2;
      logStore.add(`Found ${partitions.length} partitions (temp: ${tempDir})`);
    } catch (err) {
      logStore.add(`Partition listing failed: ${err}`, "error");
    }
  }
</script>

<div class="dump-controls">
  <!-- Root Status Badge -->
  {#if rootStatus}
    {#if hasRoot}
      <div class="root-badge root-ok">
        <span class="root-dot"></span>
        {rootStatus.message}
      </div>
    {:else}
      <div class="root-badge root-warn">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M12 9v4m0 4h.01M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z"/>
        </svg>
        Root access required
      </div>
      <p class="root-help">
        Partition dumping requires root. Root with Magisk or run <code>adb root</code> if supported.
      </p>
    {/if}
  {:else}
    <div class="root-badge root-checking">Checking root access...</div>
  {/if}

  <!-- Tab Bar -->
  <div class="tab-bar">
    <button
      class="tab"
      class:active={activeTab === "read"}
      onclick={() => (activeTab = "read")}
    >
      Read
    </button>
    <button
      class="tab"
      class:active={activeTab === "write"}
      onclick={() => (activeTab = "write")}
    >
      Write
    </button>
  </div>

  <!-- Tab Content — both mounted, display:none toggle preserves state -->
  <div class="tab-content" class:hidden={activeTab !== "read"}>
    <PartitionRead
      {hasRoot}
      {partitions}
      {tempDir}
      {freeBytes}
      {supportsShellV2}
      {isWriting}
      onReadingChange={(v) => (isReading = v)}
    />
  </div>
  <div class="tab-content" class:hidden={activeTab !== "write"}>
    <PartitionWrite
      {hasRoot}
      {partitions}
      {tempDir}
      {isReading}
      onWritingChange={(v) => (isWriting = v)}
    />
  </div>
</div>

<style>
  .dump-controls {
    display: flex;
    flex-direction: column;
    gap: 20px;
  }

  .root-badge {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    border-radius: 6px;
    padding: 6px 12px;
    font-size: var(--font-base);
    font-weight: 600;
  }

  .root-ok {
    background: rgba(var(--success-rgb), 0.1);
    border: 1px solid rgba(var(--success-rgb), 0.2);
    color: var(--success);
  }

  .root-warn {
    background: rgba(var(--warning-rgb), 0.1);
    border: 1px solid rgba(var(--warning-rgb), 0.2);
    color: var(--warning);
  }

  .root-checking {
    background: var(--input-bg);
    border: 1px solid var(--border);
    color: var(--text-muted);
  }

  .root-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--success);
  }

  .root-help {
    font-size: var(--font-base);
    color: var(--text-secondary);
    line-height: 1.5;
    margin: 0;
  }

  .root-help code {
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    background: var(--input-bg);
    padding: 2px 6px;
    border-radius: 6px;
    font-size: var(--font-sm);
  }

  .tab-bar {
    display: flex;
    gap: 0;
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

  .tab-content.hidden {
    display: none;
  }
</style>
