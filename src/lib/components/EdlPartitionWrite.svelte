<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { open } from "@tauri-apps/plugin-dialog";
  import { logStore } from "../stores/log";
  import { flashStage, flashPercent, flashMessage, ensureFlashListener } from "../stores/flash";
  import type { EdlPartitionEntry, BatchFlashResult, VerifyResult } from "../types";

  let {
    partitions,
    activeLun,
    connected,
    isReading,
    onWritingChange,
  }: {
    partitions: EdlPartitionEntry[];
    activeLun: number;
    connected: boolean;
    isReading: boolean;
    onWritingChange: (writing: boolean) => void;
  } = $props();

  // Write Partition state
  let writeFile = $state("");
  let writePartition = $state("");
  let lastVerify = $state<VerifyResult | null>(null);
  let isWriting = $state(false);
  let writeConfirm = $state(false);

  // Erase Partition state
  let erasePartition = $state("");
  let isErasing = $state(false);
  let eraseConfirm = $state(false);

  // Batch Flash state
  let rawprogramPath = $state("");
  let patchPath = $state("");
  let imageDir = $state("");
  let batchConfirm = $state(false);
  let isBatching = $state(false);
  let missingFiles = $state<string[]>([]);

  let lunPartitions = $derived(
    partitions.filter((p) => p.lun === activeLun)
  );

  let busy = $derived(isWriting || isErasing || isBatching);

  let canWrite = $derived(
    !!writePartition && !busy && !isReading && connected
  );

  let canErase = $derived(
    !!erasePartition && !busy && !isReading && connected
  );

  let canBatch = $derived(
    !!rawprogramPath && !!imageDir && !busy && !isReading && connected
  );

  async function validateBatchFiles() {
    if (!rawprogramPath || !imageDir) {
      missingFiles = [];
      return;
    }
    try {
      missingFiles = await invoke<string[]>("edl_validate_batch", {
        rawprogramPath,
        imageDir,
      });
      if (missingFiles.length > 0) {
        logStore.add(`${missingFiles.length} image file(s) missing`, "warn");
      }
    } catch { missingFiles = []; }
  }

  $effect(() => {
    if (rawprogramPath && imageDir) {
      validateBatchFiles();
    }
  });

  function setWriting(v: boolean) {
    isWriting = v;
    onWritingChange(v);
  }

  function formatSize(bytes: number): string {
    if (bytes === 0) return "0 B";
    const mb = bytes / (1024 * 1024);
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
    if (mb >= 1) return `${mb.toFixed(1)} MB`;
    return `${(bytes / 1024).toFixed(0)} KB`;
  }

  // --- Write Partition ---

  async function pickWriteFile() {
    const file = await open({
      filters: [{ name: "Image", extensions: ["img", "bin", "mbn", "elf"] }],
    });
    if (file) {
      writeFile = file as string;
      logStore.add(`Write source: ${writeFile}`);
    }
  }

  function handleWriteClick() {
    if (!writeFile) {
      pickWriteFile();
      return;
    }
    if (!writePartition || busy || isReading || !connected) return;

    if (!writeConfirm) {
      // First click — enter confirmation mode
      writeConfirm = true;
      // Auto-reset after 5 seconds
      setTimeout(() => { writeConfirm = false; }, 5000);
      return;
    }

    // Second click — execute
    writeConfirm = false;
    doWritePartition();
  }

  async function doWritePartition() {
    const part = lunPartitions.find((p) => p.name === writePartition);
    if (!part) return;

    const fileName = writeFile.split(/[\\/]/).pop() || writeFile;
    lastVerify = null;

    setWriting(true);
    ensureFlashListener();
    flashStage.set("Sending");
    flashPercent.set(null);
    flashMessage.set(`Writing ${fileName} to ${writePartition}...`);

    logStore.add(`Writing ${fileName} -> ${writePartition} (LUN ${activeLun})`);

    try {
      const verify = await invoke<VerifyResult | null>("edl_program_partition", {
        lun: activeLun,
        partitionName: writePartition,
        startSector: part.start_sector,
        numSectors: part.num_sectors,
        filePath: writeFile,
      });
      lastVerify = verify;
      flashStage.set("Complete");
      flashMessage.set(`Partition ${writePartition} written successfully`);
      logStore.add(`Partition ${writePartition} written!`);
      if (verify) {
        logStore.add(
          verify.passed
            ? `Verified: ${verify.detail}`
            : `Verification FAILED: ${verify.detail}`,
          verify.passed ? "info" : "error"
        );
      }
    } catch (err) {
      logStore.add(`Write failed: ${err}`, "error");
      flashStage.set("Error");
      flashMessage.set(`Write failed: ${err}`);
    } finally {
      setWriting(false);
    }
  }

  // --- Erase Partition ---

  function handleEraseClick() {
    if (!erasePartition || busy || isReading || !connected) return;

    if (!eraseConfirm) {
      eraseConfirm = true;
      setTimeout(() => { eraseConfirm = false; }, 5000);
      return;
    }

    eraseConfirm = false;
    doErasePartition();
  }

  async function doErasePartition() {
    const part = lunPartitions.find((p) => p.name === erasePartition);
    if (!part) return;

    isErasing = true;
    onWritingChange(true);
    ensureFlashListener();
    flashStage.set("Sending");
    flashPercent.set(null);
    flashMessage.set(`Erasing ${erasePartition}...`);

    logStore.add(`Erasing ${erasePartition} (LUN ${activeLun})`);

    try {
      await invoke<void>("edl_erase_partition", {
        lun: activeLun,
        partitionName: erasePartition,
        startSector: part.start_sector,
        numSectors: part.num_sectors,
      });
      flashStage.set("Complete");
      flashMessage.set(`Partition ${erasePartition} erased successfully`);
      logStore.add(`Partition ${erasePartition} erased!`);
    } catch (err) {
      logStore.add(`Erase failed: ${err}`, "error");
      flashStage.set("Error");
      flashMessage.set(`Erase failed: ${err}`);
    } finally {
      isErasing = false;
      onWritingChange(false);
    }
  }

  // --- Batch Flash ---

  async function pickRawprogram() {
    const file = await open({
      filters: [{ name: "XML", extensions: ["xml"] }],
    });
    if (file) {
      rawprogramPath = file as string;
      logStore.add(`rawprogram.xml: ${rawprogramPath}`);
    }
  }

  async function pickPatch() {
    const file = await open({
      filters: [{ name: "XML", extensions: ["xml"] }],
    });
    if (file) {
      patchPath = file as string;
      logStore.add(`patch.xml: ${patchPath}`);
    }
  }

  async function pickImageDir() {
    const dir = await open({ directory: true });
    if (dir) {
      imageDir = dir as string;
      logStore.add(`Image directory: ${imageDir}`);
    }
  }

  function handleBatchClick() {
    if (!canBatch) return;
    if (!batchConfirm) {
      batchConfirm = true;
      setTimeout(() => { batchConfirm = false; }, 5000);
      return;
    }
    batchConfirm = false;
    doBatchFlash();
  }

  async function doBatchFlash() {
    if (!canBatch) return;

    isBatching = true;
    onWritingChange(true);
    ensureFlashListener();
    flashStage.set("Sending");
    flashPercent.set(null);
    flashMessage.set("Batch flash in progress...");

    logStore.add("Starting batch flash...");

    try {
      const result = await invoke<BatchFlashResult>("edl_batch_flash", {
        rawprogramPath,
        patchPath: patchPath || null,
        imageDir,
      });

      flashStage.set("Complete");
      flashMessage.set("Batch flash completed");

      if (result.programmed.length > 0) {
        logStore.add(`Programmed: ${result.programmed.join(", ")}`);
      }
      if (result.erased.length > 0) {
        logStore.add(`Erased: ${result.erased.join(", ")}`);
      }
      if (result.patched > 0) {
        logStore.add(`Patches applied: ${result.patched}`);
      }
      if (result.errors.length > 0) {
        for (const e of result.errors) {
          logStore.add(`Batch error: ${e}`, "error");
        }
      }
      if (result.verified.length > 0) {
        const passed = result.verified.filter(([, ok]) => ok).length;
        const failed = result.verified.filter(([, ok]) => !ok).length;
        logStore.add(`Verified: ${passed} passed, ${failed} failed`);
        for (const [name, ok] of result.verified) {
          if (!ok) logStore.add(`Verify FAILED: ${name}`, "error");
        }
      }
      logStore.add(
        `Batch flash done: ${result.programmed.length} programmed, ${result.erased.length} erased, ${result.patched} patched, ${result.errors.length} errors`
      );
    } catch (err) {
      logStore.add(`Batch flash failed: ${err}`, "error");
      flashStage.set("Error");
      flashMessage.set(`Batch flash failed: ${err}`);
    } finally {
      isBatching = false;
      onWritingChange(false);
    }
  }
</script>

<div class="edl-partition-write">
  <!-- Write Partition: single row -->
  <div class="section">
    <div class="section-label">Write Partition</div>
    <div class="controls-row">
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="clickable-input"
        class:placeholder={!writeFile}
        onclick={pickWriteFile}
        title={writeFile || "Click to select image file"}
      >
        {writeFile || "Image file..."}
      </div>
      <select
        class="select-input"
        bind:value={writePartition}
        disabled={busy || isReading}
      >
        <option value="">Partition...</option>
        {#each lunPartitions as p}
          <option value={p.name}>{p.name} ({formatSize(p.size_bytes)})</option>
        {/each}
      </select>
      <button
        class="btn-danger"
        class:confirming={writeConfirm}
        onclick={handleWriteClick}
        disabled={!canWrite}
      >
        {isWriting ? "Writing..." : writeConfirm ? "Confirm?" : "Write"}
      </button>
    </div>
    {#if lastVerify}
      <div class="verify-badge" class:pass={lastVerify.passed} class:fail={!lastVerify.passed}>
        {lastVerify.passed ? "Verified" : "Verification Failed"}
        <span class="verify-detail">{lastVerify.detail}</span>
      </div>
    {/if}
  </div>

  <div class="divider"></div>

  <!-- Erase Partition: single row -->
  <div class="section">
    <div class="section-label">Erase Partition</div>
    <div class="controls-row">
      <select
        class="select-input"
        bind:value={erasePartition}
        disabled={busy || isReading}
      >
        <option value="">Select partition...</option>
        {#each lunPartitions as p}
          <option value={p.name}>{p.name} ({formatSize(p.size_bytes)})</option>
        {/each}
      </select>
      <button
        class="btn-danger"
        class:confirming={eraseConfirm}
        onclick={handleEraseClick}
        disabled={!canErase}
      >
        {isErasing ? "Erasing..." : eraseConfirm ? "Confirm?" : "Erase"}
      </button>
    </div>
  </div>

  <div class="divider"></div>

  <!-- Batch Flash -->
  <div class="section">
    <div class="section-label">Batch Flash</div>
    <div class="controls-row">
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="clickable-input"
        class:placeholder={!rawprogramPath}
        onclick={pickRawprogram}
        title={rawprogramPath || "Click to select rawprogram.xml"}
      >
        {rawprogramPath || "rawprogram.xml..."}
      </div>
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="clickable-input"
        class:placeholder={!patchPath}
        onclick={pickPatch}
        title={patchPath || "Click to select patch.xml (optional)"}
      >
        {patchPath || "patch.xml..."}
      </div>
    </div>
    <div class="controls-row">
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="clickable-input"
        class:placeholder={!imageDir}
        onclick={pickImageDir}
        title={imageDir || "Click to select image directory"}
      >
        {imageDir || "Image directory..."}
      </div>
      <button
        class="btn-danger"
        class:confirming={batchConfirm}
        onclick={handleBatchClick}
        disabled={!canBatch}
      >
        {isBatching ? "Flashing..." : batchConfirm ? "Confirm?" : "Batch Flash"}
      </button>
    </div>

    {#if missingFiles.length > 0}
      <div class="missing-warning">
        <strong>{missingFiles.length} file(s) missing:</strong>
        <ul class="missing-list">
          {#each missingFiles.slice(0, 5) as f}
            <li>{f}</li>
          {/each}
          {#if missingFiles.length > 5}
            <li>...and {missingFiles.length - 5} more</li>
          {/if}
        </ul>
      </div>
    {/if}
  </div>
</div>

<style>
  .edl-partition-write {
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

  .controls-row {
    display: flex;
    gap: 8px;
  }

  .clickable-input {
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

  .clickable-input:hover {
    border-color: var(--primary);
  }

  .clickable-input.placeholder {
    color: var(--text-muted);
  }

  .divider {
    border-top: 1px solid var(--border);
  }

  .select-input {
    flex: 1;
    background: var(--input-bg);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 7px 12px;
    font-size: var(--font-base);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text);
    cursor: pointer;
    box-sizing: border-box;
  }

  :global([data-theme="dark"]) .select-input {
    color-scheme: dark;
  }

  .select-input option {
    background: var(--surface);
    color: var(--text);
  }

  .select-input:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .btn-danger {
    background: var(--danger);
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

  .btn-danger:hover:not(:disabled) {
    filter: brightness(0.85);
  }

  .btn-danger:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .btn-danger.confirming {
    background: var(--danger);
    filter: brightness(0.85);
    animation: pulse-confirm 1s infinite;
  }

  @keyframes pulse-confirm {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.7; }
  }

  .verify-badge {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    border-radius: 6px;
    font-size: var(--font-sm);
    font-weight: 600;
  }

  .verify-badge.pass {
    background: rgba(var(--success-rgb), 0.1);
    color: rgba(var(--success-rgb), 1);
    border: 1px solid rgba(var(--success-rgb), 0.3);
  }

  .verify-badge.fail {
    background: rgba(var(--danger-rgb), 0.1);
    color: rgba(var(--danger-rgb), 1);
    border: 1px solid rgba(var(--danger-rgb), 0.3);
  }

  .verify-detail {
    font-weight: 400;
    font-size: var(--font-xs);
    opacity: 0.85;
  }

  .missing-warning {
    background: rgba(231, 76, 60, 0.1);
    border: 1px solid var(--danger);
    border-radius: 6px;
    padding: 10px 12px;
    font-size: var(--font-sm);
    color: var(--danger);
  }

  .missing-list {
    margin: 4px 0 0 16px;
    padding: 0;
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    font-size: var(--font-xs);
  }
</style>
