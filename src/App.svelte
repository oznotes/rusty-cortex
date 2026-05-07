<script lang="ts">
  import { getVersion } from "@tauri-apps/api/app";
  import Sidebar from "./lib/components/Sidebar.svelte";
  import Workspace from "./lib/components/Workspace.svelte";
  import BottomPanel from "./lib/components/BottomPanel.svelte";
  import ThemeToggle from "./lib/components/ThemeToggle.svelte";
  import { themeStore } from "./lib/stores/theme";
  import { currentDevice, edlConnected, edlInfo } from "./lib/stores/device";

  themeStore.init();

  let appVersion = $state("...");
  getVersion().then((v) => appVersion = v);

  let statusText = $derived.by(() => {
    if (!$currentDevice) return "No device connected";
    const name = $currentDevice.product || "Device";
    const proto = $currentDevice.protocol;
    if (proto === "Edl") {
      if ($edlConnected && $edlInfo) {
        const parts = [name];
        if ($edlInfo.storage_type) parts.push($edlInfo.storage_type.toUpperCase());
        if ($edlInfo.sector_size) parts.push(`${$edlInfo.sector_size}B`);
        if ($edlInfo.num_luns) parts.push(`${$edlInfo.num_luns} LUN${$edlInfo.num_luns > 1 ? "s" : ""}`);
        return parts.join(" — ");
      }
      return `${name} — EDL`;
    }
    return `${name} — ${proto}`;
  });
</script>

<main>
  <header class="title-bar">
    <div class="title-bar-left">
      <span class="status-text">{statusText}</span>
    </div>
    <div class="title-bar-right">
      <span class="version-badge">v{appVersion}</span>
      <ThemeToggle />
    </div>
  </header>

  <div class="app-body">
    <Sidebar />
    <div class="main-area">
      <Workspace />
      <BottomPanel />
    </div>
  </div>
</main>

<style>
  main {
    display: flex;
    flex-direction: column;
    height: 100vh;
    background: var(--bg);
    color: var(--text);
  }

  .title-bar {
    height: 40px;
    padding: 0 16px;
    display: flex;
    justify-content: space-between;
    align-items: center;
    background: #202020;
    border-top: 1px solid rgba(255, 255, 255, 0.06);
    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
    flex-shrink: 0;
  }

  .title-bar-left {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .status-text {
    font-size: var(--font-base);
    font-weight: 500;
    color: rgba(255, 255, 255, 0.85);
  }

  .title-bar-right {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .version-badge {
    font-size: var(--font-sm);
    font-weight: 500;
    color: rgba(255, 255, 255, 0.6);
    background: rgba(255, 255, 255, 0.06);
    padding: 2px 8px;
    border-radius: 6px;
  }

  .app-body {
    display: flex;
    flex: 1;
    min-height: 0;
  }

  .main-area {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
</style>
