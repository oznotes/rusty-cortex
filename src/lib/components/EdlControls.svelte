<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { open } from "@tauri-apps/plugin-dialog";
  import { edlInfo, edlPartitions, edlConnected, currentDevice } from "../stores/device";
  import { logStore } from "../stores/log";
  import type { DeviceInfo, EdlDeviceInfo, EdlPartitionEntry, ProgrammerEntry, ProgrammerCandidate } from "../types";
  import EdlPartitionRead from "./EdlPartitionRead.svelte";
  import EdlPartitionWrite from "./EdlPartitionWrite.svelte";

  let programmerPath = $state("");
  let isConnecting = $state(false);
  let activeLun = $state(0);
  let activeTab = $state<"read" | "write">("read");
  let isReading = $state(false);
  let isWriting = $state(false);
  let loadedLuns = $state<Set<number>>(new Set());

  // Database auto-detect
  let knownProgrammer = $state<ProgrammerEntry | null>(null);
  let scanResults = $state<ProgrammerCandidate[]>([]);
  let expandedCandidate = $state<string | null>(null);
  let isScanning = $state(false);
  let showSavedList = $state(false);
  let savedEntries = $state<[string, ProgrammerEntry][]>([]);

  let info = $derived($edlInfo);
  let connected = $derived($edlConnected);
  let partitions = $derived($edlPartitions);

  let isUfs = $derived(info?.storage_type === "ufs");
  let numLuns = $derived(info?.num_luns ?? 1);

  $effect(() => {
    if (info?.hw_id && info?.pk_hash && !connected) {
      lookupProgrammer(info.hw_id, info.pk_hash);
    }
  });

  async function lookupProgrammer(hwid: string, pkhash: string) {
    try {
      const entry = await invoke<ProgrammerEntry | null>("edl_db_lookup", { hwid, pkhash });
      knownProgrammer = entry;
      if (entry) {
        logStore.add(`Known programmer: ${entry.programmer_name} (used ${entry.use_count}x)`);
      }
    } catch { /* silent — best effort */ }
  }

  async function scanFolder() {
    const dir = await open({ directory: true });
    if (!dir) return;
    isScanning = true;
    try {
      const results = await invoke<ProgrammerCandidate[]>("edl_scan_programmers", {
        dirPath: dir as string,
        hwid: info?.hw_id ?? null,
        pkhash: info?.pk_hash ?? null,
      });
      scanResults = results;
      const matched = results.filter(r => r.match_level === "BinaryVerified" || r.match_level === "DbExact" || r.match_level === "FilenameMatch").length;
      logStore.add(`Found ${results.length} programmer files${matched > 0 ? ` (${matched} compatible)` : ""}`);
    } catch (err) {
      logStore.add(`Scan failed: ${err}`, "error");
    } finally {
      isScanning = false;
    }
  }

  function selectCandidate(candidate: ProgrammerCandidate) {
    programmerPath = candidate.path;
    scanResults = [];
  }

  async function autoConnect() {
    if (!knownProgrammer) return;
    programmerPath = knownProgrammer.programmer_path;
    await connectDevice();
  }

  async function loadSavedEntries() {
    try {
      savedEntries = await invoke<[string, ProgrammerEntry][]>("edl_db_list");
    } catch { /* best effort */ }
  }

  async function removeEntry(key: string) {
    try {
      await invoke<void>("edl_db_remove", { key });
      savedEntries = savedEntries.filter(([k]) => k !== key);
      if (savedEntries.length === 0) showSavedList = false;
    } catch (err) {
      logStore.add(`Remove failed: ${err}`, "error");
    }
  }

  async function pickProgrammer() {
    const path = await open({
      filters: [{ name: "Programmer", extensions: ["elf", "mbn", "bin"] }],
    });
    if (path) {
      programmerPath = path as string;
      logStore.add(`Programmer: ${programmerPath}`);
    }
  }

  async function connectDevice() {
    const resuming = info?.firehose_active === true;
    if (!resuming && !programmerPath) return;
    if (isConnecting) return;
    isConnecting = true;

    if (resuming) {
      logStore.add("Resuming Firehose session (programmer already loaded)...");
    } else {
      logStore.add("Uploading programmer and connecting...");
    }

    try {
      const result = await invoke<EdlDeviceInfo>("edl_connect", {
        programmerPath: resuming ? "" : programmerPath,
        hwid: info?.hw_id ?? null,
        pkhash: info?.pk_hash ?? null,
      });
      $edlInfo = result;
      $edlConnected = true;
      logStore.add(
        `Firehose active — ${result.storage_type ?? "unknown"} storage, sector ${result.sector_size ?? "?"}B`
      );

      // Load partitions for initial LUN
      await loadPartitions(0);
      loadedLuns = new Set([0]);
    } catch (err) {
      logStore.add(`EDL connect failed: ${err}`, "error");
    } finally {
      isConnecting = false;
    }
  }

  async function loadPartitions(lun: number) {
    try {
      const parts = await invoke<EdlPartitionEntry[]>("edl_list_partitions", {
        lun,
      });
      // Merge with existing partitions (replace entries for this LUN)
      const others = $edlPartitions.filter((p) => p.lun !== lun);
      $edlPartitions = [...others, ...parts];
      logStore.add(`LUN ${lun}: ${parts.length} partitions`);
    } catch (err) {
      logStore.add(`Partition list failed (LUN ${lun}): ${err}`, "error");
    }
  }

  async function switchLun(lun: number) {
    activeLun = lun;
    if (!loadedLuns.has(lun)) {
      await loadPartitions(lun);
      loadedLuns = new Set([...loadedLuns, lun]);
    }
  }

  async function edlReboot(mode: string) {
    try {
      logStore.add(`EDL reboot: ${mode}`);
      await invoke<void>("edl_reboot", { mode });
      logStore.add(`Reboot (${mode}) command sent`);
      if (mode === "reset" || mode === "off") {
        logStore.add("If device doesn't respond, power cycle manually");
      }
      await disconnect();

      // Post-reboot detection: wait for USB re-enumerate, then check once
      await new Promise(r => setTimeout(r, 1500));
      try {
        const device = await invoke<DeviceInfo | null>("detect_device");
        if (device) {
          $currentDevice = device;
          logStore.add(`Device found in ${device.protocol} mode — click Detect for full info`);
        } else {
          logStore.add("Device disconnected after reboot");
          $currentDevice = null;
        }
      } catch {
        logStore.add("Device disconnected after reboot");
        $currentDevice = null;
      }
    } catch (err) {
      logStore.add(`Reboot failed: ${err}`, "error");
    }
  }

  async function disconnect() {
    try {
      await invoke<void>("edl_disconnect");
    } catch { /* best effort */ }
    $edlConnected = false;
    $edlPartitions = [];
    loadedLuns = new Set();
    $edlInfo = null;
  }

  function truncateHash(hash: string | null, len: number = 16): string {
    if (!hash) return "---";
    if (hash.length <= len) return hash;
    return hash.slice(0, len) + "...";
  }
</script>

<div class="edl-controls">
  {#if !connected}
    <!-- State A: Sahara connected, no programmer -->
    {#if info}
      <div class="section-label">Device Identity (Sahara)</div>
      <div class="identity-grid">
        <span class="id-label">Serial</span>
        <span class="id-value mono">{info.serial ?? "---"}</span>
        <span class="id-label">HWID</span>
        <span class="id-value mono">{info.hw_id ?? "---"}</span>
        <span class="id-label">PKHash</span>
        <span class="id-value mono" title={info.pk_hash ?? ""}>{truncateHash(info.pk_hash, 24)}</span>
      </div>
      {#if info.firehose_active}
        <div class="firehose-badge">
          Firehose mode detected — programmer already loaded from previous session
        </div>
      {/if}
    {/if}

    {#if info?.firehose_active}
      <div class="section">
        <button
          class="btn-primary"
          onclick={connectDevice}
          disabled={isConnecting}
        >
          {isConnecting ? "Resuming..." : "Resume Session"}
        </button>
        <p class="hint-text">
          No programmer upload needed — resuming existing Firehose session.
        </p>
      </div>
    {:else if knownProgrammer}
      <div class="known-programmer">
        <div class="section-label">Known Programmer</div>
        <div class="known-info">
          <span class="known-name">{knownProgrammer.programmer_name}</span>
          <span class="known-meta">Used {knownProgrammer.use_count}x</span>
        </div>
        {#if knownProgrammer && !knownProgrammer.file_exists}
          <div class="file-warning">
            File not found — choose a different programmer
          </div>
        {/if}
        <div class="known-actions">
          <button class="btn-primary" onclick={autoConnect} disabled={isConnecting || !knownProgrammer?.file_exists}>
            {isConnecting ? "Connecting..." : "Auto-Connect"}
          </button>
          <button class="btn-secondary" onclick={() => (knownProgrammer = null)}>
            Choose Different
          </button>
        </div>
      </div>
    {:else}
      <div class="section">
        <div class="section-label">Programmer</div>
        <p class="hint-text">
          Select the signed programmer file (.elf, .mbn, or .bin) for this device. The programmer must match
          the device's chipset and PKHash.
        </p>
        <div class="file-row">
          <input
            type="text"
            readonly
            value={programmerPath || "No file selected"}
            class="file-input"
            class:placeholder={!programmerPath}
          />
          <button class="btn-secondary" onclick={pickProgrammer} disabled={isConnecting}>
            Browse
          </button>
          <button class="btn-secondary" onclick={scanFolder} disabled={isConnecting || isScanning}>
            {isScanning ? "Scanning..." : "Scan Folder"}
          </button>
        </div>

        {#if scanResults.length > 0}
          <div class="scan-results">
            {#each scanResults as candidate}
              <button
                class="scan-item"
                class:scan-item-dim={candidate.match_level === "DbOtherDevice"}
                class:scan-item-expanded={expandedCandidate === candidate.path}
                onclick={() => {
                  expandedCandidate = expandedCandidate === candidate.path ? null : candidate.path;
                }}
                ondblclick={() => selectCandidate(candidate)}
              >
                <span class="scan-name">{candidate.name}</span>
                <span class="scan-size">{(candidate.size_bytes / 1024).toFixed(0)} KB</span>
                {#if candidate.match_level === "BinaryVerified"}
                  <span class="badge-verified">Verified</span>
                {:else if candidate.match_level === "DbExact"}
                  <span class="badge-compatible">Known Compatible</span>
                {:else if candidate.match_level === "FilenameMatch"}
                  <span class="badge-likely">Likely Match</span>
                {:else if candidate.match_level === "DbOtherDevice"}
                  <span class="badge-other">Other Device</span>
                {/if}
                {#if !candidate.valid}
                  <span class="scan-warn">invalid</span>
                {/if}
              </button>
              {#if expandedCandidate === candidate.path}
                <div class="scan-detail">
                  {#if candidate.identity}
                    {#if candidate.identity.chipset || candidate.identity.msm_id}
                      <div class="detail-row">
                        <span class="detail-label">Chipset</span>
                        <span class="detail-value">{candidate.identity.chipset ?? `0x${candidate.identity.msm_id.toString(16).toUpperCase()}`}</span>
                      </div>
                    {/if}
                    {#if candidate.identity.hw_id}
                      <div class="detail-row">
                        <span class="detail-label">HWID</span>
                        <span class="detail-value detail-mono">{candidate.identity.hw_id}{#if candidate.identity.hwid_from_filename}<span class="detail-source"> (from filename)</span>{/if}</span>
                      </div>
                      <div class="detail-row">
                        <span class="detail-label">OEM ID</span>
                        <span class="detail-value detail-mono">{candidate.identity.oem_id.toString(16).padStart(4, '0')}</span>
                      </div>
                    {/if}
                    <div class="detail-row">
                      <span class="detail-label">PKHash ({candidate.identity.hash_algorithm === "Sha256" ? "SHA-256" : "SHA-384"})</span>
                      <span class="detail-value detail-mono" title={candidate.identity.pk_hash}>{candidate.identity.pk_hash.substring(0, 32)}...</span>
                    </div>
                    {#if $edlInfo?.hw_id}
                      <div class="detail-row">
                        <span class="detail-label">Match</span>
                        <span class="detail-value">
                          {#if candidate.match_level === "BinaryVerified"}
                            <span class="match-ok">PKHash verified</span>
                          {:else}
                            <span style="color: var(--text-muted)">Not verified</span>
                          {/if}
                        </span>
                      </div>
                    {/if}
                  {:else}
                    <div class="detail-row">
                      <span class="detail-label">Identity</span>
                      <span class="detail-value" style="color: var(--text-muted)">Could not parse (not a signed Qualcomm programmer)</span>
                    </div>
                  {/if}
                  <div class="detail-row">
                    <span class="detail-label">Path</span>
                    <span class="detail-value detail-mono" title={candidate.path}>{candidate.path}</span>
                  </div>
                </div>
              {/if}
            {/each}
          </div>
        {/if}

        <button
          class="btn-primary"
          onclick={connectDevice}
          disabled={!programmerPath || isConnecting}
        >
          {isConnecting ? "Connecting..." : "Connect"}
        </button>

        <button
          class="saved-toggle"
          onclick={() => { showSavedList = !showSavedList; if (showSavedList) loadSavedEntries(); }}
        >
          {showSavedList ? "Hide Saved Programmers" : "Saved Programmers"}
        </button>

        {#if showSavedList}
          <div class="saved-list">
            {#each savedEntries as [key, entry]}
              <div class="saved-row">
                <span class="saved-key mono">{key}</span>
                <span class="saved-name">{entry.programmer_name}</span>
                <button class="btn-icon-danger" onclick={() => removeEntry(key)} title="Remove">x</button>
              </div>
            {:else}
              <span class="hint-text">No saved programmers</span>
            {/each}
          </div>
        {/if}
      </div>
    {/if}

  {:else}
    <!-- State B: Firehose active -->

    <!-- LUN tabs (only for UFS with multiple LUNs) -->
    {#if isUfs && numLuns > 1}
      <div class="lun-bar">
        {#each Array(numLuns) as _, i}
          <button
            class="lun-tab"
            class:active={activeLun === i}
            onclick={() => switchLun(i)}
          >
            LUN {i}
          </button>
        {/each}
      </div>
    {/if}

    <!-- Read / Write tab bar -->
    <div class="tab-bar">
      <button class="tab" class:active={activeTab === "read"} onclick={() => (activeTab = "read")}>Read</button>
      <button class="tab" class:active={activeTab === "write"} onclick={() => (activeTab = "write")}>Write</button>
    </div>

    <!-- Tab content — both mounted, display:none toggle preserves state -->
    <div class="tab-content" class:hidden={activeTab !== "read"}>
      <EdlPartitionRead {partitions} {activeLun} {connected} {isWriting} onReadingChange={(v) => (isReading = v)} />
    </div>
    <div class="tab-content" class:hidden={activeTab !== "write"}>
      <EdlPartitionWrite {partitions} {activeLun} {connected} {isReading} onWritingChange={(v) => (isWriting = v)} />
    </div>

  {/if}
</div>

<style>
  .edl-controls {
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

  .hint-text {
    font-size: var(--font-base);
    color: var(--text-muted);
    line-height: 1.5;
    margin: 0;
  }

  /* Identity grid (Sahara info) */
  .identity-grid {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 4px 12px;
    align-items: baseline;
  }

  .id-label {
    font-size: var(--font-sm);
    color: var(--text-muted);
  }

  .id-value {
    font-size: var(--font-sm);
    color: var(--text-secondary);
  }

  .id-value.mono {
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
  }


  /* LUN tabs */
  .lun-bar {
    display: flex;
    gap: 0;
    border-bottom: 1px solid var(--border);
  }

  .lun-tab {
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

  .lun-tab:hover {
    color: var(--text-secondary);
  }

  .lun-tab.active {
    color: var(--primary);
    border-bottom-color: var(--primary);
  }

  /* Read / Write tab bar */
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

  /* Fields */
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

  /* Buttons */
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

  /* Firehose mode indicator */
  .firehose-badge {
    padding: 8px 12px;
    border-radius: 6px;
    font-size: var(--font-sm);
    font-weight: 500;
    background: rgba(var(--warning-rgb), 0.12);
    color: var(--warning);
    border: 1px solid rgba(var(--warning-rgb), 0.3);
  }

  /* Known programmer (auto-detect match) */
  .known-programmer {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 12px;
    border: 1px solid rgba(var(--success-rgb), 0.4);
    border-radius: 6px;
    background: rgba(var(--success-rgb), 0.06);
  }

  .known-info {
    display: flex;
    align-items: baseline;
    gap: 8px;
  }

  .known-name {
    font-size: var(--font-base);
    font-weight: 600;
    color: var(--text);
  }

  .known-meta {
    font-size: var(--font-xs);
    color: var(--text-muted);
  }

  .file-warning {
    font-size: var(--font-sm);
    color: var(--danger);
    font-weight: 500;
    padding: 4px 0;
  }

  .known-actions {
    display: flex;
    gap: 8px;
  }

  /* Scan results */
  .scan-results {
    display: flex;
    flex-direction: column;
    gap: 2px;
    max-height: 200px;
    overflow-y: auto;
    border: 1px solid var(--border);
    border-radius: 6px;
  }

  .scan-item {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    background: none;
    border: none;
    cursor: pointer;
    text-align: left;
    transition: background 0.15s;
  }

  .scan-item:hover {
    background: var(--surface-hover);
  }

  .scan-name {
    flex: 1;
    font-size: var(--font-sm);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text);
  }

  .scan-size {
    font-size: var(--font-xs);
    color: var(--text-muted);
  }

  .scan-warn {
    font-size: var(--font-xs);
    font-weight: 600;
    text-transform: uppercase;
    color: rgba(var(--danger-rgb), 0.85);
  }

  .badge-compatible {
    font-size: var(--font-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    padding: 2px 8px;
    border-radius: 6px;
    background: rgba(var(--success-rgb), 0.15);
    color: var(--success);
    white-space: nowrap;
  }

  .badge-likely {
    font-size: var(--font-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    padding: 2px 8px;
    border-radius: 6px;
    background: rgba(var(--warning-rgb), 0.15);
    color: var(--warning);
    white-space: nowrap;
  }

  .badge-other {
    font-size: var(--font-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    padding: 2px 8px;
    border-radius: 6px;
    background: var(--surface-hover);
    color: var(--text-muted);
    white-space: nowrap;
  }

  .badge-verified {
    font-size: var(--font-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    padding: 2px 8px;
    border-radius: 6px;
    background: rgba(var(--success-rgb), 0.25);
    color: var(--success);
    white-space: nowrap;
    border: 1px solid rgba(var(--success-rgb), 0.4);
  }

  .scan-item-expanded {
    background: var(--surface-hover);
  }

  .scan-detail {
    padding: 4px 12px 8px 12px;
    border-bottom: 1px solid var(--border);
    background: var(--surface-hover);
  }

  .detail-row {
    display: flex;
    gap: 12px;
    padding: 2px 0;
    font-size: var(--font-xs);
  }

  .detail-label {
    width: 60px;
    flex-shrink: 0;
    color: var(--text-muted);
  }

  .detail-value {
    color: var(--text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .detail-mono {
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
  }

  .match-ok {
    color: var(--success);
  }

  .match-fail {
    color: rgba(var(--danger-rgb), 0.85);
  }

  .detail-source {
    color: var(--text-muted);
    font-family: "Inter", sans-serif;
    font-style: italic;
    margin-left: 4px;
  }

  .scan-item-dim {
    opacity: 0.4;
  }

  /* Saved programmers */
  .saved-toggle {
    background: none;
    border: none;
    font-size: var(--font-sm);
    color: var(--text-muted);
    cursor: pointer;
    padding: 0;
    text-align: left;
    text-decoration: underline;
    text-decoration-style: dotted;
    transition: color 0.15s;
  }

  .saved-toggle:hover {
    color: var(--text-secondary);
  }

  .saved-list {
    display: flex;
    flex-direction: column;
    gap: 4px;
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px;
  }

  .saved-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 0;
  }

  .saved-key {
    font-size: var(--font-xs);
    color: var(--text-muted);
    flex-shrink: 0;
    max-width: 120px;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .saved-name {
    flex: 1;
    font-size: var(--font-sm);
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .btn-icon-danger {
    background: none;
    border: none;
    color: rgba(var(--danger-rgb), 0.7);
    font-size: var(--font-sm);
    font-weight: 600;
    cursor: pointer;
    padding: 2px 6px;
    border-radius: 6px;
    transition: background 0.15s, color 0.15s;
    flex-shrink: 0;
  }

  .btn-icon-danger:hover {
    background: rgba(var(--danger-rgb), 0.1);
    color: rgba(var(--danger-rgb), 1);
  }
</style>
