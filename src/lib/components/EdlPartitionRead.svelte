<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { open } from "@tauri-apps/plugin-dialog";
  import { logStore } from "../stores/log";
  import { flashStage, flashPercent, flashMessage, ensureFlashListener } from "../stores/flash";
  import type { EdlPartitionEntry } from "../types";
  import GptViewer from "./GptViewer.svelte";

  let {
    partitions,
    activeLun,
    connected,
    isWriting,
    onReadingChange,
  }: {
    partitions: EdlPartitionEntry[];
    activeLun: number;
    connected: boolean;
    isWriting: boolean;
    onReadingChange: (reading: boolean) => void;
  } = $props();

  let isReading = $state(false);
  let selected = $state<Set<string>>(new Set());
  let outputDir = $state("");
  let filter = $state("");

  let lunPartitions = $derived(
    partitions.filter((p) => p.lun === activeLun)
  );

  let selectedCount = $derived(selected.size);

  let totalSelectedSize = $derived.by(() => {
    let total = 0;
    for (const name of selected) {
      const p = partitions.find((pp) => pp.name === name);
      if (p) total += p.size_bytes;
    }
    return total;
  });

  let canRead = $derived(
    selectedCount > 0 && !isReading && !isWriting && connected
  );

  $effect(() => {
    activeLun;
    selected = new Set();
  });

  function setReading(v: boolean) {
    isReading = v;
    onReadingChange(v);
  }

  function togglePartition(name: string) {
    const next = new Set(selected);
    if (next.has(name)) {
      next.delete(name);
    } else {
      next.add(name);
    }
    selected = next;
  }

  async function pickOutputDir() {
    const dir = await open({ directory: true });
    if (dir) {
      outputDir = dir as string;
      logStore.add(`Output: ${outputDir}`);
    }
    return !!outputDir;
  }

  function formatSize(bytes: number): string {
    if (bytes === 0) return "0 B";
    const mb = bytes / (1024 * 1024);
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
    if (mb >= 1) return `${mb.toFixed(1)} MB`;
    return `${(bytes / 1024).toFixed(0)} KB`;
  }

  async function readSelected() {
    if (!canRead) return;

    // Auto-open folder picker if none selected
    if (!outputDir) {
      const picked = await pickOutputDir();
      if (!picked) return;
    }

    setReading(true);
    ensureFlashListener();
    flashStage.set("Sending");
    flashPercent.set(null);

    const names = Array.from(selected);
    logStore.add(`Reading ${names.length} partition(s) from LUN ${activeLun}`);

    for (const name of names) {
      const part = lunPartitions.find((p) => p.name === name);
      if (!part) continue;

      const outPath = `${outputDir}/${name}.img`;
      flashMessage.set(`Reading ${name}...`);

      try {
        await invoke<void>("edl_read_partition", {
          lun: activeLun,
          partitionName: name,
          startSector: part.start_sector,
          numSectors: part.num_sectors,
          outputPath: outPath,
        });
        logStore.add(`Read ${name} complete`);
      } catch (err) {
        logStore.add(`Read ${name} failed: ${err}`, "error");
        flashStage.set("Error");
        flashMessage.set(`Read failed: ${err}`);
        setReading(false);
        return;
      }
    }

    flashStage.set("Complete");
    flashMessage.set(`${names.length} partition(s) read successfully`);
    logStore.add("All selected partitions read");
    setReading(false);
  }
</script>

<div class="edl-partition-read">
  <div class="section">
    <div class="section-header">
      <div class="section-label">
        Partitions
        <span class="count-badge">{lunPartitions.length}</span>
      </div>
      {#if selectedCount > 0}
        <span class="selection-info">
          {selectedCount} selected &middot; {formatSize(totalSelectedSize)}
        </span>
      {/if}
    </div>

    <!-- Controls row: filter + output directory + read button -->
    <div class="controls-row">
      <input
        type="text"
        class="filter-input"
        placeholder="Filter..."
        bind:value={filter}
      />
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="dir-input"
        class:placeholder={!outputDir}
        onclick={pickOutputDir}
        title={outputDir || "Click to select output directory"}
      >
        {outputDir || "Output folder..."}
      </div>
      <button class="btn-primary" onclick={readSelected} disabled={!canRead}>
        {isReading ? "Reading..." : `Read${selectedCount > 0 ? ` ${selectedCount}` : ""}`}
      </button>
    </div>

    <GptViewer
      partitions={lunPartitions}
      {selected}
      disabled={isReading || isWriting}
      onToggle={togglePartition}
      bind:filter
    />
  </div>
</div>

<style>
  .edl-partition-read {
    display: flex;
    flex-direction: column;
    gap: 20px;
  }

  .section {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .section-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .section-label {
    font-size: var(--font-sm);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-label);
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .count-badge {
    font-size: var(--font-xs);
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 6px;
    background: rgba(var(--primary-rgb), 0.1);
    color: var(--primary);
    letter-spacing: normal;
    text-transform: none;
  }

  .selection-info {
    font-size: var(--font-sm);
    color: var(--text-muted);
  }

  .controls-row {
    display: flex;
    gap: 8px;
  }

  .filter-input {
    flex: 1;
    background: var(--input-bg);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 7px 12px;
    font-size: var(--font-base);
    color: var(--text);
  }

  .filter-input::placeholder {
    color: var(--text-muted);
  }

  .dir-input {
    flex: 1;
    background: var(--input-bg);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 7px 12px;
    font-size: var(--font-base);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text);
    cursor: pointer;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    transition: border-color 0.15s;
  }

  .dir-input:hover {
    border-color: var(--primary);
  }

  .dir-input.placeholder {
    color: var(--text-muted);
  }

  .btn-primary {
    background: var(--primary);
    color: white;
    border: none;
    border-radius: 6px;
    padding: 7px 16px;
    font-size: var(--font-base);
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s;
    white-space: nowrap;
    flex-shrink: 0;
  }

  .btn-primary:hover:not(:disabled) {
    background: var(--primary-hover);
  }

  .btn-primary:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
