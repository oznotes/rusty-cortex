<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { open, save, confirm } from "@tauri-apps/plugin-dialog";
  import { logStore } from "../stores/log";
  import { flashStage, flashPercent, flashMessage, ensureFlashListener } from "../stores/flash";
  import type { PartitionInfo } from "../types";

  let {
    hasRoot,
    partitions,
    tempDir,
    freeBytes,
    supportsShellV2,
    isWriting,
    onReadingChange,
  }: {
    hasRoot: boolean;
    partitions: PartitionInfo[];
    tempDir: string;
    freeBytes: number | null;
    supportsShellV2: boolean;
    isWriting: boolean;
    onReadingChange: (reading: boolean) => void;
  } = $props();

  let filter = $state("");
  let selected = $state<Set<string>>(new Set());
  let outputDir = $state("");
  let isReading = $state(false);
  let gridExpanded = $state(true);

  // Full image dump fields
  let blockDevice = $state("/dev/block/sda");
  let offset = $state("0");
  let size = $state("");
  let imageSavePath = $state("");

  let filteredPartitions = $derived(
    filter
      ? partitions.filter((p) =>
          p.name.toLowerCase().includes(filter.toLowerCase())
        )
      : partitions
  );

  let selectedCount = $derived(selected.size);

  let totalSelectedSize = $derived.by(() => {
    let total = 0;
    for (const name of selected) {
      const p = partitions.find((pp) => pp.name === name);
      if (p?.size_bytes) total += p.size_bytes;
    }
    return total;
  });

  let canDumpPartitions = $derived(
    selectedCount > 0 && !!outputDir && !isReading && !isWriting && hasRoot
  );

  let canDumpImage = $derived(
    !!blockDevice && !!imageSavePath && !isReading && !isWriting && hasRoot
  );

  let largestSelectedSize = $derived.by(() => {
    let max = 0;
    for (const name of selected) {
      const p = partitions.find((pp) => pp.name === name);
      if (p?.size_bytes && p.size_bytes > max) max = p.size_bytes;
    }
    return max;
  });

  let spaceWarning = $derived.by(() => {
    if (supportsShellV2) return "";
    if (freeBytes === null || largestSelectedSize === 0) return "";
    if (largestSelectedSize > freeBytes) {
      const largest = formatTotalSize(largestSelectedSize);
      const avail = formatTotalSize(freeBytes);
      return `Largest partition (${largest}) exceeds ${avail} free on device temp`;
    }
    return "";
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

  function selectAll() {
    selected = new Set(filteredPartitions.map((p) => p.name));
  }

  function clearSelection() {
    selected = new Set();
  }

  async function pickOutputDir() {
    const dir = await open({ directory: true });
    if (dir) {
      outputDir = dir as string;
      logStore.add(`Output: ${outputDir}`);
    }
  }

  async function pickImageSavePath() {
    const path = await save({ defaultPath: "dump.img" });
    if (path) {
      imageSavePath = path as string;
      logStore.add(`Image save to: ${imageSavePath}`);
    }
  }

  async function doDumpPartitions() {
    if (!canDumpPartitions) return;

    const names = Array.from(selected);
    const partitionsWithSizes: [string, number | null][] = names.map((name) => {
      const p = partitions.find((pp) => pp.name === name);
      return [name, p?.size_bytes ?? null];
    });

    let needDump: string[] = names;
    try {
      needDump = await invoke<string[]>("check_dump_resume", {
        outputDir,
        partitions: partitionsWithSizes,
      });
    } catch (e) {
      logStore.add(`Resume check failed, dumping all selected: ${e}`);
    }

    const skipCount = names.length - needDump.length;
    if (skipCount > 0) {
      logStore.add(`Skipping ${skipCount} already read partition(s), reading ${needDump.length} remaining`);
    }

    if (needDump.length === 0) {
      logStore.add("All selected partitions already read.");
      return;
    }

    const candidatePaths = needDump.map((name) => `${outputDir}/${name}.img`);
    let existingFiles: string[] = [];
    try {
      const existing = await invoke<string[]>("check_files_exist", { paths: candidatePaths });
      existingFiles = existing.map((p) => p.split(/[\\/]/).pop() ?? p);
    } catch (e) {
      logStore.add(`Overwrite check failed: ${e}`);
    }

    if (existingFiles.length > 0) {
      const proceed = await confirm(
        `${existingFiles.length} file(s) already exist and will be overwritten:\n${existingFiles.slice(0, 5).join(", ")}${existingFiles.length > 5 ? `... and ${existingFiles.length - 5} more` : ""}`,
        { title: "Overwrite Files?", kind: "warning" }
      );
      if (!proceed) return;
    }

    setReading(true);
    ensureFlashListener();
    flashStage.set("Sending");
    flashPercent.set(null);

    logStore.add(`Reading ${needDump.length} partitions to ${outputDir}`);

    try {
      await invoke<void>("dump_partitions", {
        partitions: needDump,
        outputDir: outputDir,
        tempDir: tempDir,
      });
      logStore.add("Partition dump completed!");
    } catch (err) {
      logStore.add(`Read failed: ${err}`, "error");
      flashStage.set("Error");
      flashMessage.set(`Read failed: ${err}`);
    } finally {
      setReading(false);
    }
  }

  async function doDumpImage() {
    if (!canDumpImage) return;
    setReading(true);
    ensureFlashListener();
    flashStage.set("Sending");
    flashPercent.set(null);

    const offsetNum = Math.max(0, parseInt(offset, 10) || 0);
    const sizeNum = size ? Math.max(1, parseInt(size, 10) || 0) : null;

    if (sizeNum !== null && sizeNum <= 0) {
      logStore.add("Size must be a positive number", "error");
      setReading(false);
      return;
    }

    logStore.add(
      `Image dump: ${blockDevice} offset=${offsetNum} size=${sizeNum ?? "end"}`
    );

    try {
      await invoke<void>("dump_image", {
        device: blockDevice,
        offset: offsetNum,
        size: sizeNum,
        localPath: imageSavePath,
        tempDir: tempDir,
      });
      logStore.add("Image dump completed!");
    } catch (err) {
      logStore.add(`Read failed: ${err}`, "error");
      flashStage.set("Error");
      flashMessage.set(`Read failed: ${err}`);
    } finally {
      setReading(false);
    }
  }

  function formatTotalSize(bytes: number): string {
    if (bytes === 0) return "";
    const mb = bytes / (1024 * 1024);
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
    return `${mb.toFixed(0)} MB`;
  }
</script>

<div class="partition-read">
  <!-- Collapsible Partition Grid -->
  <div class="section">
    <div class="section-label">Read Partitions</div>

    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div class="grid-toggle" onclick={() => (gridExpanded = !gridExpanded)}>
      <svg
        class="chevron"
        class:expanded={gridExpanded}
        width="12"
        height="12"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2.5"
      >
        <polyline points="9 18 15 12 9 6" />
      </svg>
      <span>
        <strong>{partitions.length}</strong> partitions
        {#if selectedCount > 0}
          &middot; <strong>{selectedCount}</strong> selected
          {#if formatTotalSize(totalSelectedSize)}
            &middot; {formatTotalSize(totalSelectedSize)}
          {/if}
        {/if}
      </span>
    </div>

    {#if gridExpanded}
      <div class="filter-row">
        <input
          type="text"
          class="filter-input"
          placeholder="Filter partitions..."
          bind:value={filter}
          disabled={!hasRoot}
        />
        <button class="link-btn" onclick={selectAll} disabled={!hasRoot}>Select All</button>
        <span class="link-sep">|</span>
        <button class="link-btn muted" onclick={clearSelection} disabled={!hasRoot}>Clear</button>
      </div>

      <div class="partition-list" class:disabled={!hasRoot}>
        {#if partitions.length === 0 && hasRoot}
          <div class="list-empty">Loading partitions...</div>
        {:else if filteredPartitions.length === 0 && filter}
          <div class="list-empty">No partitions match "{filter}"</div>
        {:else}
          {#each filteredPartitions as p}
            <label class="partition-item">
              <input
                type="checkbox"
                checked={selected.has(p.name)}
                onchange={() => togglePartition(p.name)}
                disabled={!hasRoot || isReading || isWriting}
              />
              <span class="partition-name">{p.name}</span>
              <span class="partition-size">{p.size_display}</span>
            </label>
          {/each}
        {/if}
      </div>
    {/if}

    <div class="field">
      <div class="field-label">Save to</div>
      <div class="file-row">
        <input
          type="text"
          readonly
          value={outputDir || "No directory selected"}
          class="file-input"
          class:placeholder={!outputDir}
        />
        <button class="btn-secondary" onclick={pickOutputDir} disabled={!hasRoot || isReading || isWriting}>
          Browse
        </button>
      </div>
    </div>

    {#if spaceWarning}
      <div class="space-warn">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M12 9v4m0 4h.01M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z"/>
        </svg>
        {spaceWarning}
      </div>
    {/if}

    <button class="btn-primary" onclick={doDumpPartitions} disabled={!canDumpPartitions}>
      {isReading ? "Reading..." : `Read ${selectedCount} Partition${selectedCount !== 1 ? "s" : ""}`}
    </button>
  </div>

  <!-- Divider -->
  <div class="divider"></div>

  <!-- Full Image Dump -->
  <div class="section">
    <div class="section-label">Read Full Image</div>

    <div class="field">
      <div class="field-label">Block device</div>
      <input
        type="text"
        class="path-input"
        bind:value={blockDevice}
        placeholder="/dev/block/sda"
        disabled={!hasRoot || isReading || isWriting}
      />
    </div>

    <div class="field-row">
      <div class="field field-half">
        <div class="field-label">Offset (bytes)</div>
        <input
          type="text"
          class="path-input"
          bind:value={offset}
          placeholder="0"
          disabled={!hasRoot || isReading || isWriting}
        />
      </div>
      <div class="field field-half">
        <div class="field-label">Size (bytes, empty = full)</div>
        <input
          type="text"
          class="path-input"
          bind:value={size}
          placeholder="empty = to end"
          disabled={!hasRoot || isReading || isWriting}
        />
      </div>
    </div>

    <div class="field">
      <div class="field-label">Save to</div>
      <div class="file-row">
        <input
          type="text"
          readonly
          value={imageSavePath || "No file selected"}
          class="file-input"
          class:placeholder={!imageSavePath}
        />
        <button class="btn-secondary" onclick={pickImageSavePath} disabled={!hasRoot || isReading || isWriting}>
          Save As
        </button>
      </div>
    </div>

    <button class="btn-primary" onclick={doDumpImage} disabled={!canDumpImage}>
      {isReading ? "Reading..." : "Read Image"}
    </button>
  </div>
</div>

<style>
  .partition-read {
    display: flex;
    flex-direction: column;
    gap: 20px;
  }

  .section {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .section-label {
    font-size: var(--font-sm);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-label);
  }

  .grid-toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px 12px;
    cursor: pointer;
    font-size: var(--font-base);
    color: var(--text-secondary);
    transition: background 0.15s;
  }

  .grid-toggle:hover {
    background: var(--surface-hover);
  }

  .grid-toggle strong {
    color: var(--text);
    font-weight: 600;
  }

  .chevron {
    flex-shrink: 0;
    transition: transform 0.15s;
  }

  .chevron.expanded {
    transform: rotate(90deg);
  }

  .filter-row {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .filter-input {
    flex: 1;
    max-width: 240px;
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

  .link-btn {
    font-size: var(--font-base);
    color: var(--primary);
    background: none;
    border: none;
    cursor: pointer;
    font-weight: 500;
    padding: 0;
  }

  .link-btn.muted {
    color: var(--text-muted);
  }

  .link-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .link-sep {
    color: var(--border);
  }

  .partition-list {
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px 12px;
    max-height: 200px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }

  .partition-list.disabled {
    opacity: 0.4;
    pointer-events: none;
  }

  .list-empty {
    text-align: center;
    color: var(--text-muted);
    font-size: var(--font-base);
    padding: 16px 0;
  }

  .partition-item {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--font-base);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text-secondary);
    padding: 2px 0;
    cursor: pointer;
  }

  .partition-item input[type="checkbox"] {
    accent-color: var(--primary);
  }

  .partition-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .partition-size {
    color: var(--text-muted);
    font-size: var(--font-xs);
    white-space: nowrap;
  }

  .divider {
    border-top: 1px solid var(--border);
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .field-label {
    font-size: var(--font-base);
    font-weight: 500;
    color: var(--text-secondary);
  }

  .field-row {
    display: flex;
    gap: 12px;
  }

  .field-half {
    flex: 1;
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

  .path-input {
    background: var(--input-bg);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 9px 12px;
    font-size: var(--font-base);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text);
    width: 100%;
    box-sizing: border-box;
  }

  .path-input::placeholder {
    color: var(--text-muted);
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
    align-self: flex-start;
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
    padding: 9px 16px;
    font-size: var(--font-base);
    font-weight: 500;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
    white-space: nowrap;
  }

  .btn-secondary:hover:not(:disabled) {
    background: var(--surface-hover);
    color: var(--text);
  }

  .btn-secondary:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .space-warn {
    display: flex;
    align-items: center;
    gap: 8px;
    background: rgba(var(--warning-rgb), 0.1);
    border: 1px solid rgba(var(--warning-rgb), 0.2);
    border-radius: 6px;
    padding: 8px 12px;
    font-size: var(--font-base);
    color: var(--warning);
  }
</style>
