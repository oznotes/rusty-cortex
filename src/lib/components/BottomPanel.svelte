<script lang="ts">
  import LogPanel from "./LogPanel.svelte";
  import ShellTerminal from "./ShellTerminal.svelte";
  import { currentDevice } from "../stores/device";

  function safeParseHeight(key: string, fallback: number): number {
    const val = parseInt(localStorage.getItem(key) || String(fallback));
    return Number.isFinite(val) && val > 0 ? val : fallback;
  }

  let activeTab = $state<"log" | "terminal">("log");
  let panelHeight = $state(safeParseHeight("bottomPanelHeight", 200));

  let hasAdb = $derived($currentDevice?.protocol === "Adb");
  let isMinimized = $derived(panelHeight <= 40);

  function onTabClick(tab: "log" | "terminal") {
    if (isMinimized) {
      // Expand to last known height or default
      const saved = safeParseHeight("bottomPanelLastHeight", 200);
      panelHeight = Math.max(saved, 100);
      localStorage.setItem("bottomPanelHeight", String(panelHeight));
    } else if (activeTab === tab) {
      // Clicking active tab minimizes
      localStorage.setItem("bottomPanelLastHeight", String(panelHeight));
      panelHeight = 32;
      localStorage.setItem("bottomPanelHeight", "32");
    }
    activeTab = tab;
  }

  function startDrag(e: MouseEvent) {
    e.preventDefault();
    const startY = e.clientY;
    const startHeight = panelHeight;

    function onMove(e: MouseEvent) {
      const delta = startY - e.clientY;
      const maxHeight = Math.max(100, Math.floor(window.innerHeight * 0.6));
      const newHeight = Math.max(32, Math.min(maxHeight, startHeight + delta));
      panelHeight = newHeight < 50 ? 32 : newHeight;
    }

    function onUp() {
      localStorage.setItem("bottomPanelHeight", String(panelHeight));
      if (panelHeight > 40) {
        localStorage.setItem("bottomPanelLastHeight", String(panelHeight));
      }
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    }

    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }
</script>

<div class="bottom-panel" style="height: {panelHeight}px">
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div class="drag-handle" onmousedown={startDrag} role="separator" aria-orientation="horizontal">
    <div class="drag-indicator"></div>
  </div>

  <div class="tab-bar">
    <button
      class="tab"
      class:active={activeTab === "log"}
      onclick={() => onTabClick("log")}
    >
      Log
    </button>
    {#if hasAdb}
      <button
        class="tab"
        class:active={activeTab === "terminal"}
        onclick={() => onTabClick("terminal")}
      >
        Terminal
      </button>
    {/if}
  </div>

  {#if !isMinimized}
    <div class="panel-content">
      <div class="content-pane" class:hidden={activeTab !== "log"}>
        <LogPanel />
      </div>
      {#if hasAdb}
        <div class="content-pane" class:hidden={activeTab !== "terminal"}>
          <ShellTerminal />
        </div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .bottom-panel {
    border-top: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    flex-shrink: 0;
    min-height: 32px;
  }

  .drag-handle {
    height: 4px;
    cursor: ns-resize;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 4px 0;
    background: var(--surface);
    user-select: none;
  }

  .drag-handle:hover .drag-indicator {
    background: var(--text-muted);
  }

  .drag-indicator {
    width: 32px;
    height: 2px;
    border-radius: 1px;
    background: var(--border);
    transition: background 0.15s;
  }

  .tab-bar {
    display: flex;
    gap: 0;
    background: var(--surface);
    border-bottom: 1px solid var(--border);
    padding: 0 16px;
    flex-shrink: 0;
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
    padding: 8px 12px;
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

  .panel-content {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }

  .content-pane {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }

  .content-pane.hidden {
    display: none;
  }
</style>
