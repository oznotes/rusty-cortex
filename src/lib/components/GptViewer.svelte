<script lang="ts">
  import type { EdlPartitionEntry } from "../types";

  let {
    partitions,
    selected,
    disabled,
    onToggle,
    filter = $bindable(""),
  }: {
    partitions: EdlPartitionEntry[];
    selected: Set<string>;
    disabled: boolean;
    onToggle: (name: string) => void;
    filter?: string;
  } = $props();
  let sortBy = $state<"start_sector" | "name" | "size_bytes">("start_sector");
  let sortAsc = $state(true);
  let highlightedPartition = $state<string | null>(null);
  let expandedPartition = $state<string | null>(null);

  let filteredPartitions = $derived.by(() => {
    let list = filter
      ? partitions.filter((p) =>
          p.name.toLowerCase().includes(filter.toLowerCase())
        )
      : [...partitions];

    list.sort((a, b) => {
      let cmp = 0;
      if (sortBy === "name") cmp = a.name.localeCompare(b.name);
      else if (sortBy === "size_bytes") cmp = a.size_bytes < b.size_bytes ? -1 : a.size_bytes > b.size_bytes ? 1 : 0;
      else cmp = a.start_sector < b.start_sector ? -1 : a.start_sector > b.start_sector ? 1 : 0;
      return sortAsc ? cmp : -cmp;
    });
    return list;
  });

  let barSegments = $derived.by(() => {
    if (partitions.length === 0) return [];
    const logSizes = partitions.map((p) => Math.log2(Math.max(p.size_bytes, 1)));
    const total = logSizes.reduce((a, b) => a + b, 0);
    return partitions.map((p, i) => ({
      name: p.name,
      category: p.category,
      size: p.size_bytes,
      widthPercent: total > 0 ? (logSizes[i] / total) * 100 : 100 / partitions.length,
    }));
  });

  function categoryColor(cat: string): string {
    const colors: Record<string, string> = {
      boot: "var(--partition-boot)",
      system: "var(--partition-system)",
      firmware: "var(--partition-firmware)",
      userdata: "var(--partition-userdata)",
      metadata: "var(--partition-metadata)",
      unknown: "var(--partition-unknown)",
    };
    return colors[cat] || colors.unknown;
  }

  function toggleSort(col: "start_sector" | "name" | "size_bytes") {
    if (sortBy === col) {
      sortAsc = !sortAsc;
    } else {
      sortBy = col;
      sortAsc = true;
    }
  }

  function formatSize(bytes: number): string {
    if (bytes === 0) return "0 B";
    const mb = bytes / (1024 * 1024);
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
    if (mb >= 1) return `${mb.toFixed(1)} MB`;
    return `${(bytes / 1024).toFixed(0)} KB`;
  }

  function formatAttributes(attrs: number): string {
    const flags: string[] = [];
    if (attrs & 1) flags.push("platform-required");
    if (attrs & 4) flags.push("bootable");
    // Bits 48-63 are type-specific; JS bitwise ops truncate to 32-bit,
    // so use division instead of shift for high bits
    const typeSpecific = Math.floor(attrs / 2**48) & 0xFFFF;
    if (typeSpecific) flags.push(`type:0x${typeSpecific.toString(16)}`);
    return flags.join(", ") || "none";
  }

  function sortIndicator(col: string): string {
    if (sortBy !== col) return "";
    return sortAsc ? " \u25B2" : " \u25BC";
  }
</script>

<div class="gpt-viewer">
  <!-- Visual Partition Bar -->
  <div class="partition-bar" role="img" aria-label="Partition layout">
    {#each barSegments as seg}
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="bar-segment"
        class:highlighted={highlightedPartition === seg.name}
        style="width: max({seg.widthPercent}%, 20px); background: {categoryColor(seg.category)}"
        title="{seg.name} — {formatSize(seg.size)}"
        onclick={() => { highlightedPartition = seg.name; }}
        onmouseenter={() => { highlightedPartition = seg.name; }}
        onmouseleave={() => { highlightedPartition = null; }}
      ></div>
    {/each}
  </div>

  <!-- Partition Table -->
  <div class="table-wrapper">
    <table class="partition-table">
      <thead>
        <tr>
          <th class="col-check"></th>
          <th class="col-dot"></th>
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <th class="col-name sortable" onclick={() => toggleSort("name")}>
            Name{sortIndicator("name")}
          </th>
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <th class="col-size sortable" onclick={() => toggleSort("size_bytes")}>
            Size{sortIndicator("size_bytes")}
          </th>
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <th class="col-lba sortable" onclick={() => toggleSort("start_sector")}>
            Start LBA{sortIndicator("start_sector")}
          </th>
        </tr>
      </thead>
      <tbody>
        {#each filteredPartitions as p}
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <tr
            class="partition-row"
            class:highlighted={highlightedPartition === p.name}
            class:expanded={expandedPartition === p.name}
            onclick={() => {
              highlightedPartition = p.name;
              expandedPartition = expandedPartition === p.name ? null : p.name;
            }}
          >
            <td class="col-check">
              <input
                type="checkbox"
                checked={selected.has(p.name)}
                onclick={(e) => { e.stopPropagation(); }}
                onchange={() => onToggle(p.name)}
                {disabled}
              />
            </td>
            <td class="col-dot">
              <span class="category-dot" style="background: {categoryColor(p.category)}"></span>
            </td>
            <td class="col-name">{p.name}</td>
            <td class="col-size">{formatSize(p.size_bytes)}</td>
            <td class="col-lba">{p.start_sector}</td>
          </tr>
          {#if expandedPartition === p.name}
            <tr class="detail-row">
              <td colspan="5">
                <div class="detail-grid">
                  <span class="detail-label">Type GUID</span>
                  <span class="detail-value">{p.type_guid}</span>
                  <span class="detail-label">Unique GUID</span>
                  <span class="detail-value">{p.unique_guid}</span>
                  <span class="detail-label">Sectors</span>
                  <span class="detail-value">{p.start_sector} – {p.start_sector + p.num_sectors - 1} ({p.num_sectors} sectors)</span>
                  <span class="detail-label">Flags</span>
                  <span class="detail-value">{formatAttributes(p.attributes)}</span>
                  <span class="detail-label">Raw Attributes</span>
                  <span class="detail-value">0x{p.attributes.toString(16).padStart(16, "0")}</span>
                </div>
              </td>
            </tr>
          {/if}
        {/each}
      </tbody>
    </table>
  </div>
</div>

<style>
  .gpt-viewer {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .partition-bar {
    display: flex;
    height: 28px;
    border-radius: 6px;
    overflow: hidden;
    border: 1px solid var(--border);
    background: var(--input-bg);
  }

  .bar-segment {
    min-width: 4px;
    height: 100%;
    opacity: 0.75;
    transition: opacity 0.15s;
    cursor: pointer;
    border-right: 1px solid rgba(0, 0, 0, 0.15);
  }

  .bar-segment:last-child {
    border-right: none;
  }

  .bar-segment:hover,
  .bar-segment.highlighted {
    opacity: 1;
  }

  .table-wrapper {
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 6px;
    max-height: 300px;
    overflow-y: auto;
  }

  .partition-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--font-base);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
  }

  .partition-table thead {
    position: sticky;
    top: 0;
    background: var(--surface);
    z-index: 1;
  }

  .partition-table th {
    text-align: left;
    padding: 6px 8px;
    font-size: var(--font-sm);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-label);
    border-bottom: 1px solid var(--border);
    white-space: nowrap;
  }

  .partition-table th.sortable {
    cursor: pointer;
    user-select: none;
  }

  .partition-table th.sortable:hover {
    color: var(--text);
  }

  .partition-table td {
    padding: 4px 8px;
    color: var(--text-secondary);
    border-bottom: 1px solid var(--border);
  }

  .partition-row {
    cursor: pointer;
    transition: background 0.1s;
  }

  .partition-row:hover,
  .partition-row.highlighted {
    background: var(--surface-hover);
  }

  .partition-row.expanded {
    background: var(--surface-hover);
  }

  .col-check {
    width: 32px;
    text-align: center;
  }

  .col-check input[type="checkbox"] {
    accent-color: var(--primary);
  }

  .col-dot {
    width: 24px;
    text-align: center;
  }

  .category-dot {
    display: inline-block;
    width: 10px;
    height: 10px;
    border-radius: 50%;
  }

  .col-name {
    font-weight: 500;
    color: var(--text);
  }

  .col-size {
    white-space: nowrap;
  }

  .col-lba {
    white-space: nowrap;
    font-size: var(--font-xs);
  }

  .detail-row td {
    padding: 0;
    background: var(--surface);
  }

  .detail-grid {
    display: grid;
    grid-template-columns: 120px 1fr;
    gap: 4px 12px;
    padding: 8px 36px;
    font-size: var(--font-sm);
  }

  .detail-label {
    color: var(--text-muted);
    font-weight: 500;
  }

  .detail-value {
    color: var(--text-secondary);
    word-break: break-all;
  }

  :global(:root) {
    --partition-boot: #4a90d9;
    --partition-system: #50b77d;
    --partition-firmware: #e8944a;
    --partition-userdata: #9b6dc6;
    --partition-metadata: #7a8599;
    --partition-unknown: #555b6e;
  }

  :global([data-theme="light"]) {
    --partition-boot: #3a7cc0;
    --partition-system: #3d9464;
    --partition-firmware: #d47d2e;
    --partition-userdata: #8254b0;
    --partition-metadata: #6b7585;
    --partition-unknown: #4a5060;
  }
</style>
