<script lang="ts">
  import { flashStage, flashMessage, flashPercent } from "../stores/flash";
</script>

{#if $flashStage !== "Idle"}
  <div class="progress-container">
    <div class="progress-meta">
      <span class="progress-status"
        class:complete={$flashStage === "Complete"}
        class:error={$flashStage === "Error"}
      >
        {$flashMessage}
      </span>
      {#if $flashPercent !== null && $flashStage === "Sending"}
        <span class="progress-percent">{Math.round($flashPercent)}%</span>
      {/if}
    </div>
    <div class="progress-track">
      <div
        class="progress-fill"
        class:determinate={$flashPercent !== null && ($flashStage === "Sending" || $flashStage === "Flashing")}
        class:active={$flashPercent === null && ($flashStage === "Sending" || $flashStage === "Flashing" || $flashStage === "Validating")}
        class:complete={$flashStage === "Complete"}
        class:error={$flashStage === "Error"}
        style={$flashPercent !== null && ($flashStage === "Sending" || $flashStage === "Flashing") ? `width: ${$flashPercent}%` : ""}
      ></div>
    </div>
  </div>
{/if}

<style>
  .progress-container {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin-top: 16px;
  }

  .progress-meta {
    display: flex;
    justify-content: space-between;
  }

  .progress-status {
    font-size: var(--font-base);
    font-weight: 500;
    color: var(--text-secondary);
  }

  .progress-status.complete {
    color: var(--success);
  }

  .progress-status.error {
    color: var(--danger);
  }

  .progress-percent {
    font-size: var(--font-base);
    font-weight: 600;
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text-secondary);
  }

  .progress-track {
    height: 4px;
    background: var(--input-bg);
    border-radius: 2px;
    overflow: hidden;
  }

  .progress-fill {
    height: 100%;
    border-radius: 2px;
    transition: width 0.3s ease;
  }

  .progress-fill.determinate {
    background: var(--primary);
    animation: none;
  }

  .progress-fill.active {
    background: var(--primary);
    width: 100%;
    animation: indeterminate 1.5s infinite ease-in-out;
  }

  .progress-fill.complete {
    background: var(--success);
    width: 100%;
    animation: none;
  }

  .progress-fill.error {
    background: var(--danger);
    width: 100%;
    animation: none;
  }

  @keyframes indeterminate {
    0% { transform: translateX(-100%); }
    50% { transform: translateX(0%); }
    100% { transform: translateX(100%); }
  }
</style>
