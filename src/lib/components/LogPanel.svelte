<script lang="ts">
  import { logStore } from "../stores/log";
  import { tick } from "svelte";

  let logContainer = $state<HTMLDivElement>();

  $effect(() => {
    if ($logStore.length && logContainer) {
      const el = logContainer;
      tick().then(() => {
        el.scrollTop = el.scrollHeight;
      });
    }
  });

  function formatTime(date: Date): string {
    return date.toLocaleTimeString("en-US", { hour12: false });
  }

  function selectAll() {
    if (logContainer) {
      const range = document.createRange();
      range.selectNodeContents(logContainer);
      const sel = window.getSelection();
      if (sel) {
        sel.removeAllRanges();
        sel.addRange(range);
      }
    }
  }

  function copyLog() {
    const text = $logStore
      .map((e) => `${formatTime(e.timestamp)} › ${e.message}`)
      .join("\n");
    navigator.clipboard.writeText(text);
  }
</script>

<div class="log-panel">
  <div class="log-actions">
    <button class="action-btn" onclick={selectAll}>Select All</button>
    <button class="action-btn" onclick={copyLog}>Copy</button>
    <button class="action-btn" onclick={() => logStore.clear()}>Clear</button>
  </div>

  <div class="log-content" bind:this={logContainer}>
    {#if $logStore.length === 0}
      <div class="log-empty">Waiting for activity...</div>
    {:else}
      {#each $logStore as entry}
        <div class="log-entry" class:warn={entry.level === "warn"} class:error={entry.level === "error"}>
          <span class="log-time">{formatTime(entry.timestamp)}</span>
          <span class="log-chevron" class:error-chevron={entry.level === "error"} class:warn-chevron={entry.level === "warn"}>›</span>
          <span class="log-msg">{entry.message}</span>
        </div>
      {/each}
    {/if}
  </div>
</div>

<style>
  .log-panel {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }

  .log-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    padding: 4px 12px;
  }

  .action-btn {
    font-size: var(--font-sm);
    padding: 2px 8px;
    border: none;
    border-radius: 6px;
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    transition: background 0.15s;
  }

  .action-btn:hover {
    background: var(--surface-hover);
  }

  .log-content {
    background: var(--log-bg);
    padding: 4px 16px;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    font-size: var(--font-sm);
    line-height: 1.8;
  }

  .log-empty {
    color: var(--text-muted);
  }

  .log-entry {
    color: var(--log-text);
    white-space: pre-wrap;
    word-break: break-all;
  }

  .log-entry.warn { color: var(--warning); }
  .log-entry.error { color: var(--danger); }

  .log-time {
    color: var(--text-muted);
    font-size: var(--font-xs);
    margin-right: 8px;
  }

  .log-chevron { color: var(--primary); margin-right: 6px; }
  .log-chevron.error-chevron { color: var(--danger); }
  .log-chevron.warn-chevron { color: var(--warning); }
</style>
