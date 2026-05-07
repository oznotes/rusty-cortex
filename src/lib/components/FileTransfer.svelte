<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { open, save } from "@tauri-apps/plugin-dialog";
  import { currentDevice } from "../stores/device";
  import { logStore } from "../stores/log";
  import { flashStage, flashPercent, ensureFlashListener } from "../stores/flash";
  import DeviceBrowser from "./DeviceBrowser.svelte";

  // Push state
  let pushLocalPath = $state("");
  let pushRemotePath = $state("/sdcard/");
  let isTransferring = $state(false);

  // Pull state
  let pullRemotePath = $state("");
  let pullLocalPath = $state("");

  // Browser state
  let browserOpen = $state(false);
  let browserMode = $state<"file" | "directory">("file");
  let browserTarget = $state<"push" | "pull">("push");

  function openBrowser(target: "push" | "pull") {
    browserTarget = target;
    browserMode = target === "push" ? "directory" : "file";
    browserOpen = true;
  }

  function onBrowserSelect(path: string) {
    if (browserTarget === "push") {
      pushRemotePath = path.endsWith("/") ? path : path + "/";
      // Re-append filename if we have one
      if (pushLocalPath) {
        const filename = pushLocalPath.split(/[\\/]/).pop() || "";
        if (filename && !pushRemotePath.endsWith(filename)) {
          pushRemotePath = pushRemotePath + filename;
        }
      }
    } else {
      pullRemotePath = path;
    }
    browserOpen = false;
  }

  // Install state
  let apkPath = $state("");
  let flagReplace = $state(true);
  let flagDowngrade = $state(false);
  let flagGrantAll = $state(false);

  async function pickPushFile() {
    const selected = await open({
      multiple: false,
      filters: [{ name: "All files", extensions: ["*"] }],
    });
    if (selected) {
      pushLocalPath = selected as string;
      const filename = pushLocalPath.split(/[\\/]/).pop() || "";
      if (pushRemotePath === "/sdcard/" || pushRemotePath.endsWith("/")) {
        pushRemotePath = pushRemotePath + filename;
      }
      logStore.add(`Selected: ${pushLocalPath}`);
    }
  }

  async function pickPullDestination() {
    const filename = pullRemotePath.split("/").pop() || "file";
    const selected = await save({ defaultPath: filename });
    if (selected) {
      pullLocalPath = selected as string;
      logStore.add(`Save to: ${pullLocalPath}`);
    }
  }

  async function pickApk() {
    const selected = await open({
      multiple: false,
      filters: [{ name: "APK files", extensions: ["apk"] }],
    });
    if (selected) {
      apkPath = selected as string;
      logStore.add(`Selected APK: ${apkPath}`);
    }
  }

  async function doPush() {
    if (!canPush) return;
    isTransferring = true;
    ensureFlashListener();
    flashStage.set("Sending");
    flashPercent.set(0);
    logStore.add(`Push: ${pushLocalPath} -> ${pushRemotePath}`);
    try {
      await invoke<void>("push_file", { localPath: pushLocalPath, remotePath: pushRemotePath });
      logStore.add("Push completed!");
    } catch (err) {
      logStore.add(`Push failed: ${err}`, "error");
      flashStage.set("Error");
    } finally {
      isTransferring = false;
    }
  }

  async function doPull() {
    if (!canPull) return;
    isTransferring = true;
    ensureFlashListener();
    flashStage.set("Sending");
    flashPercent.set(0);
    logStore.add(`Pull: ${pullRemotePath} -> ${pullLocalPath}`);
    try {
      await invoke<void>("pull_file", { remotePath: pullRemotePath, localPath: pullLocalPath });
      logStore.add("Pull completed!");
    } catch (err) {
      logStore.add(`Pull failed: ${err}`, "error");
      flashStage.set("Error");
    } finally {
      isTransferring = false;
    }
  }

  async function doInstall() {
    if (!canInstall) return;
    isTransferring = true;
    ensureFlashListener();
    flashStage.set("Sending");
    flashPercent.set(0);
    logStore.add(`Install: ${apkPath} (replace=${flagReplace}, downgrade=${flagDowngrade}, grant=${flagGrantAll})`);
    try {
      await invoke<void>("install_apk", {
        apkPath,
        replace: flagReplace,
        downgrade: flagDowngrade,
        grantAll: flagGrantAll,
      });
      logStore.add("Install completed!");
    } catch (err) {
      logStore.add(`Install failed: ${err}`, "error");
      flashStage.set("Error");
    } finally {
      isTransferring = false;
    }
  }

  let inRecovery = $derived(
    $currentDevice?.adb_state === "Recovery" || $currentDevice?.adb_state === "Sideload"
  );

  let canPush = $derived(!!pushLocalPath && !!pushRemotePath && !!$currentDevice && !isTransferring);
  let canPull = $derived(!!pullRemotePath && !!pullLocalPath && !!$currentDevice && !isTransferring);
  let canInstall = $derived(!!apkPath && !!$currentDevice && !isTransferring);
</script>

<div class="file-transfer">
  <!-- PUSH ROW -->
  <div class="action-row" class:disabled={isTransferring}>
    <div class="row-label">PUSH</div>
    <div class="row-fields">
      <input
        type="text"
        readonly
        value={pushLocalPath || "Select file..."}
        class="row-input"
        class:placeholder={!pushLocalPath}
        onclick={pickPushFile}
      />
      <span class="row-arrow">&#8594;</span>
      <input
        type="text"
        class="row-input mono"
        bind:value={pushRemotePath}
        placeholder="/sdcard/"
        disabled={isTransferring}
      />
      <button class="row-browse" onclick={() => openBrowser("push")} disabled={isTransferring} title="Browse device">
        &#128193;
      </button>
      <button class="row-btn-primary" onclick={doPush} disabled={!canPush}>
        {isTransferring ? "..." : "Push"}
      </button>
    </div>
  </div>

  <!-- PULL ROW -->
  <div class="action-row" class:disabled={isTransferring}>
    <div class="row-label">PULL</div>
    <div class="row-fields">
      <input
        type="text"
        class="row-input mono"
        bind:value={pullRemotePath}
        placeholder="/sdcard/file"
        disabled={isTransferring}
      />
      <button class="row-browse" onclick={() => openBrowser("pull")} disabled={isTransferring} title="Browse device">
        &#128193;
      </button>
      <span class="row-arrow">&#8594;</span>
      <input
        type="text"
        readonly
        value={pullLocalPath || "Save to..."}
        class="row-input"
        class:placeholder={!pullLocalPath}
        onclick={pickPullDestination}
      />
      <button class="row-btn-primary" onclick={doPull} disabled={!canPull}>
        {isTransferring ? "..." : "Pull"}
      </button>
    </div>
  </div>

  {#if !inRecovery}
  <!-- INSTALL ROW -->
  <div class="action-row" class:disabled={isTransferring}>
    <div class="row-label">INSTALL</div>
    <div class="row-fields">
      <input
        type="text"
        readonly
        value={apkPath || "Select APK..."}
        class="row-input"
        class:placeholder={!apkPath}
        onclick={pickApk}
      />
      <div class="flag-group">
        <label class="flag" title="Replace existing app"><input type="checkbox" bind:checked={flagReplace} disabled={isTransferring} /> -r</label>
        <label class="flag" title="Allow version downgrade"><input type="checkbox" bind:checked={flagDowngrade} disabled={isTransferring} /> -d</label>
        <label class="flag" title="Grant all runtime permissions"><input type="checkbox" bind:checked={flagGrantAll} disabled={isTransferring} /> -g</label>
      </div>
      <button class="row-btn-primary" onclick={doInstall} disabled={!canInstall}>
        {isTransferring ? "..." : "Install"}
      </button>
    </div>
  </div>
  {/if}

  <DeviceBrowser
    open={browserOpen}
    mode={browserMode}
    onselect={onBrowserSelect}
    onclose={() => browserOpen = false}
  />
</div>

<style>
  .file-transfer {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .action-row {
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 10px 12px;
    display: flex;
    align-items: center;
    gap: 12px;
    transition: opacity 0.15s;
  }

  .action-row.disabled {
    opacity: 0.5;
    pointer-events: none;
  }

  .row-label {
    color: var(--text-label);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    font-size: var(--font-sm);
    min-width: 52px;
    flex-shrink: 0;
  }

  .row-fields {
    display: flex;
    align-items: center;
    gap: 8px;
    flex: 1;
    min-width: 0;
  }

  .row-input {
    flex: 1;
    min-width: 0;
    background: var(--input-bg);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 7px 10px;
    font-size: var(--font-base);
    color: var(--text);
  }

  .row-input.mono {
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
  }

  .row-input.placeholder {
    color: var(--text-muted);
  }

  .row-input::placeholder {
    color: var(--text-muted);
  }

  .row-input[readonly] {
    cursor: pointer;
  }

  .row-input[readonly]:hover {
    border-color: var(--primary);
  }

  .row-arrow {
    color: var(--primary);
    font-size: var(--font-md);
    flex-shrink: 0;
  }

  .flag-group {
    display: flex;
    gap: 8px;
    flex-shrink: 0;
  }

  .flag {
    color: var(--text-muted);
    font-size: var(--font-base);
    display: flex;
    align-items: center;
    gap: 3px;
    cursor: pointer;
    white-space: nowrap;
  }

  .flag input[type="checkbox"] {
    accent-color: var(--primary);
    width: 12px;
    height: 12px;
  }

  .row-btn-primary {
    background: var(--primary);
    color: white;
    border: none;
    border-radius: 6px;
    padding: 7px 0;
    width: 64px;
    text-align: center;
    font-size: var(--font-base);
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s;
    white-space: nowrap;
    flex-shrink: 0;
  }

  .row-btn-primary:hover:not(:disabled) {
    background: var(--primary-hover);
  }

  .row-btn-primary:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .row-browse {
    background: none;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    padding: 5px 8px;
    cursor: pointer;
    font-size: var(--font-md);
    transition: background 0.15s;
    flex-shrink: 0;
    line-height: 1;
  }

  .row-browse:hover:not(:disabled) {
    background: var(--surface-hover);
  }

  .row-browse:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

</style>
