<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { open, confirm } from "@tauri-apps/plugin-dialog";
  import { logStore } from "../stores/log";
  import { flashStage, flashPercent, flashMessage, ensureFlashListener } from "../stores/flash";
  import type { PartitionInfo } from "../types";

  let {
    hasRoot,
    partitions,
    tempDir,
    isReading,
    onWritingChange,
  }: {
    hasRoot: boolean;
    partitions: PartitionInfo[];
    tempDir: string;
    isReading: boolean;
    onWritingChange: (writing: boolean) => void;
  } = $props();

  let writeFile = $state("");
  let writePartition = $state("");
  let isWriting = $state(false);

  let canWrite = $derived(
    !!writeFile && !!writePartition && !isWriting && !isReading && hasRoot
  );

  function setWriting(v: boolean) {
    isWriting = v;
    onWritingChange(v);
  }

  async function pickWriteFile() {
    const file = await open({
      filters: [{ name: "Image", extensions: ["img", "bin"] }],
    });
    if (file) {
      writeFile = file as string;
      logStore.add(`Write source: ${writeFile}`);
    }
  }

  async function doWritePartition() {
    if (!canWrite) return;

    const fileName = writeFile.split(/[\\/]/).pop() || writeFile;
    const proceed = await confirm(
      `This will OVERWRITE partition "${writePartition}" with "${fileName}".\n\nThis operation is destructive and cannot be undone. Are you sure?`,
      { title: "Write Partition", kind: "warning" }
    );
    if (!proceed) return;

    setWriting(true);
    ensureFlashListener();
    flashStage.set("Sending");
    flashPercent.set(null);

    logStore.add(`Writing ${fileName} \u2192 ${writePartition}`);

    try {
      await invoke<void>("write_partition", {
        partition: writePartition,
        imagePath: writeFile,
        tempDir: tempDir,
      });
      logStore.add(`Partition ${writePartition} written!`);
    } catch (err) {
      logStore.add(`Write failed: ${err}`, "error");
      flashStage.set("Error");
      flashMessage.set(`Write failed: ${err}`);
    } finally {
      setWriting(false);
    }
  }
</script>

<div class="partition-write">
  <div class="section">
    <div class="section-label">Write Partition</div>

    <div class="field">
      <div class="field-label">Image file</div>
      <div class="file-row">
        <input
          type="text"
          readonly
          value={writeFile || "No file selected"}
          class="file-input"
          class:placeholder={!writeFile}
        />
        <button class="btn-secondary" onclick={pickWriteFile} disabled={!hasRoot || isWriting || isReading}>
          Browse
        </button>
      </div>
    </div>

    <div class="field">
      <div class="field-label">Target partition</div>
      <select
        class="partition-select"
        bind:value={writePartition}
        disabled={!hasRoot || isWriting || isReading}
      >
        <option value="">Select partition...</option>
        {#each partitions as p}
          <option value={p.name}>{p.name} ({p.size_display})</option>
        {/each}
      </select>
    </div>

    <button class="btn-danger" onclick={doWritePartition} disabled={!canWrite}>
      {isWriting ? "Writing..." : `Write to ${writePartition || "..."}`}
    </button>
  </div>
</div>

<style>
  .partition-write {
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

  .partition-select {
    background: var(--input-bg);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 9px 12px;
    font-size: var(--font-base);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text);
    width: 100%;
    box-sizing: border-box;
    cursor: pointer;
  }

  .partition-select:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .btn-danger {
    background: var(--danger);
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

  .btn-danger:hover:not(:disabled) {
    filter: brightness(0.85);
  }

  .btn-danger:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
