<script lang="ts">
  import { themeStore } from "../stores/theme";
</script>

<button
  class="theme-toggle"
  onclick={() => themeStore.toggle()}
  title={$themeStore === "dark" ? "Switch to light theme" : "Switch to dark theme"}
>
  {#if $themeStore === "dark"}
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none">
      <circle cx="8" cy="8" r="3.5" stroke="currentColor" stroke-width="1.5" fill="none"/>
      <path d="M8 1v2M8 13v2M1 8h2M13 8h2M3.05 3.05l1.41 1.41M11.54 11.54l1.41 1.41M3.05 12.95l1.41-1.41M11.54 4.46l1.41-1.41" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
    </svg>
  {:else}
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none">
      <path d="M14 9.5A6.5 6.5 0 016.5 2 5.5 5.5 0 1014 9.5z" stroke="currentColor" stroke-width="1.5" fill="none"/>
    </svg>
  {/if}
</button>

<style>
  .theme-toggle {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    border: none;
    border-radius: 6px;
    background: rgba(255, 255, 255, 0.06);
    color: rgba(255, 255, 255, 0.75);
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
  }

  .theme-toggle:hover {
    background: rgba(255, 255, 255, 0.12);
    color: rgba(255, 255, 255, 0.85);
  }
</style>
