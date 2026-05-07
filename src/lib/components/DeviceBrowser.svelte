<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";

  type Props = {
    open: boolean;
    mode: "file" | "directory";
    onselect: (path: string) => void;
    onclose: () => void;
  };

  let { open, mode, onselect, onclose }: Props = $props();

  let currentPath = $state("/sdcard");
  let entries = $state<Array<[string, boolean, string]>>([]);
  let loading = $state(false);
  let error = $state("");

  $effect(() => {
    if (open) {
      loadDirectory(currentPath);
    }
  });

  async function loadDirectory(path: string) {
    loading = true;
    error = "";
    try {
      entries = await invoke<Array<[string, boolean, string]>>("list_device_directory", { path });
      currentPath = path;
    } catch (err) {
      error = String(err);
      entries = [];
    } finally {
      loading = false;
    }
  }

  function navigateUp() {
    const parent = currentPath.split("/").slice(0, -1).join("/") || "/";
    loadDirectory(parent);
  }

  function onEntryClick(name: string, isDir: boolean) {
    const fullPath = currentPath === "/" ? `/${name}` : `${currentPath}/${name}`;
    if (isDir) {
      loadDirectory(fullPath);
    } else if (mode === "file") {
      onselect(fullPath);
    }
  }

  function selectCurrentDir() {
    onselect(currentPath);
  }

  function handleBackdropClick(e: MouseEvent) {
    if ((e.target as HTMLElement).classList.contains("modal-backdrop")) {
      onclose();
    }
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") onclose();
  }
</script>

{#if open}
  <div class="modal-backdrop" onclick={handleBackdropClick} onkeydown={handleKeydown} role="dialog" tabindex="-1">
    <div class="modal">
      <div class="modal-header">
        <span class="modal-title">Device Browser</span>
        <button class="modal-close" onclick={onclose}>&#10005;</button>
      </div>

      <div class="breadcrumb">
        <button class="crumb-btn" onclick={() => loadDirectory("/")}>
          /
        </button>
        {#each currentPath.split("/").filter(Boolean) as segment, i}
          <span class="crumb-sep">/</span>
          <button class="crumb-btn" onclick={() => loadDirectory("/" + currentPath.split("/").filter(Boolean).slice(0, i + 1).join("/"))}>
            {segment}
          </button>
        {/each}
        <button class="nav-up" onclick={navigateUp} title="Go up">&#8593;</button>
      </div>

      <div class="file-list">
        {#if loading}
          <div class="list-msg">Loading...</div>
        {:else if error}
          <div class="list-msg error">{error}</div>
        {:else if entries.length === 0}
          <div class="list-msg">Empty directory</div>
        {:else}
          {#each entries as [name, isDir, size]}
            <button
              class="file-entry"
              class:dir={isDir}
              ondblclick={() => onEntryClick(name, isDir)}
              onclick={() => { if (!isDir && mode === "file") onselect(currentPath === "/" ? `/${name}` : `${currentPath}/${name}`); }}
            >
              <span class="file-icon">{isDir ? "/" : ""}</span>
              <span class="file-name">{name}</span>
              <span class="file-size">{size}</span>
            </button>
          {/each}
        {/if}
      </div>

      {#if mode === "directory"}
        <div class="modal-footer">
          <button class="select-dir-btn" onclick={selectCurrentDir}>
            Select this folder
          </button>
        </div>
      {/if}
    </div>
  </div>
{/if}

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
  }

  .modal {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 6px;
    width: 480px;
    max-height: 70vh;
    display: flex;
    flex-direction: column;
  }

  .modal-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
  }

  .modal-title {
    font-size: var(--font-md);
    font-weight: 600;
    color: var(--text);
  }

  .modal-close {
    background: none;
    border: none;
    color: var(--text-muted);
    cursor: pointer;
    font-size: var(--font-md);
    padding: 4px;
    border-radius: 6px;
    transition: background 0.15s;
  }

  .modal-close:hover {
    background: var(--surface-hover);
    color: var(--text);
  }

  .breadcrumb {
    display: flex;
    align-items: center;
    padding: 8px 16px;
    gap: 4px;
    border-bottom: 1px solid var(--border);
    flex-wrap: wrap;
  }

  .crumb-btn {
    background: none;
    border: none;
    color: var(--primary);
    cursor: pointer;
    font-size: var(--font-base);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    padding: 2px 4px;
    border-radius: 6px;
    transition: background 0.15s;
  }

  .crumb-btn:hover {
    background: var(--surface-hover);
  }

  .crumb-sep {
    color: var(--text-muted);
    font-size: var(--font-base);
  }

  .nav-up {
    margin-left: auto;
    background: none;
    border: 1px solid var(--border-strong);
    color: var(--text-secondary);
    cursor: pointer;
    font-size: var(--font-sm);
    padding: 2px 8px;
    border-radius: 6px;
    transition: background 0.15s;
  }

  .nav-up:hover {
    background: var(--surface-hover);
  }

  .file-list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 4px 0;
  }

  .list-msg {
    padding: 24px 16px;
    text-align: center;
    color: var(--text-muted);
    font-size: var(--font-base);
  }

  .list-msg.error {
    color: var(--danger);
  }

  .file-entry {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 6px 16px;
    background: none;
    border: none;
    color: var(--text);
    cursor: pointer;
    font-size: var(--font-base);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    text-align: left;
    transition: background 0.1s;
  }

  .file-entry:hover {
    background: var(--surface-hover);
  }

  .file-entry.dir {
    color: var(--primary);
    font-weight: 500;
  }

  .file-icon {
    flex-shrink: 0;
    font-size: var(--font-md);
    color: var(--text-muted);
  }

  .file-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .file-size {
    color: var(--text-muted);
    font-size: var(--font-xs);
    flex-shrink: 0;
  }

  .modal-footer {
    padding: 12px 16px;
    border-top: 1px solid var(--border);
  }

  .select-dir-btn {
    background: var(--primary);
    color: white;
    border: none;
    border-radius: 6px;
    padding: 8px 20px;
    font-size: var(--font-base);
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s;
    width: 100%;
  }

  .select-dir-btn:hover {
    background: var(--primary-hover);
  }
</style>
