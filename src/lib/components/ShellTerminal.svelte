<script lang="ts">
  import { onMount } from "svelte";
  import { Terminal } from "@xterm/xterm";
  import { FitAddon } from "@xterm/addon-fit";
  import { invoke, Channel } from "@tauri-apps/api/core";
  import { currentDevice, deviceReady } from "../stores/device";
  import { logStore } from "../stores/log";
  import { themeStore } from "../stores/theme";
  import { terminalColorStore, terminalColorPresets, getPresetTheme } from "../stores/terminalColor";
  import type { TerminalColorPreset } from "../stores/terminalColor";
  import type { ShellOutput } from "../types";
  import "@xterm/xterm/css/xterm.css";

  let termContainer: HTMLDivElement;
  let term: Terminal | null = null;
  let fitAddon: FitAddon | null = null;
  let currentSessionId: string | null = null;
  let resizeObserver: ResizeObserver | null = null;
  let resizeTimeout: number | null = null;
  let lineBuffer = "";
  let isLogcatActive = $state(false);
  let logcatSessionId = $state("");
  let logcatLevel = $state("V");

  const darkTheme = {
    background: "#161b2e",
    foreground: "#e5e7eb",
    cursor: "#4361ee",
    cursorAccent: "#161b2e",
    selectionBackground: "rgba(67, 97, 238, 0.3)",
    black: "#1e2233",
    red: "#e74c3c",
    green: "#2ecc71",
    yellow: "#f39c12",
    blue: "#4361ee",
    magenta: "#9b59b6",
    cyan: "#1abc9c",
    white: "#e5e7eb",
  };

  const lightTheme = {
    background: "#f6f8fa",
    foreground: "#24292f",
    cursor: "#4361ee",
    cursorAccent: "#f6f8fa",
    selectionBackground: "rgba(67, 97, 238, 0.2)",
    black: "#24292f",
    red: "#dc2626",
    green: "#16a34a",
    yellow: "#ca8a04",
    blue: "#4361ee",
    magenta: "#9333ea",
    cyan: "#0891b2",
    white: "#f6f8fa",
  };

  function getTermTheme(theme: string, colorPreset: TerminalColorPreset = "default") {
    const preset = getPresetTheme(colorPreset);
    if (preset) return preset;
    return theme === "dark" ? darkTheme : lightTheme;
  }

  async function openSession(serial: string, deviceName: string | null, adbState: string | null) {
    if (!term) return;
    lineBuffer = "";

    // Close existing session if any
    if (currentSessionId) {
      await invoke<void>("shell_close", { sessionId: currentSessionId }).catch(() => {});
      currentSessionId = null;
    }

    const sessionId = crypto.randomUUID();
    currentSessionId = sessionId;

    const output = new Channel<ShellOutput>();
    output.onmessage = (msg: ShellOutput) => {
      if (!term) return;
      if (msg.kind === "Data") {
        term.write(new Uint8Array(msg.data));
      } else if (msg.kind === "Stderr") {
        term.write(new Uint8Array(msg.data));
      } else if (msg.kind === "Exit") {
        const codeStr = msg.code !== undefined ? ` (exit ${msg.code})` : "";
        term.writeln(`\r\n\x1b[90m[${msg.message}${codeStr}]\x1b[0m`);
        currentSessionId = null;
      }
    };

    try {
      await invoke<void>("shell_open", {
        serial,
        sessionId,
        onOutput: output,
      });
      logStore.add(`Shell session opened: ${serial}`);
      if (adbState === "Recovery" || adbState === "Sideload") {
        term.writeln(`\x1b[33mConnected to ${deviceName || serial} (${adbState} mode). Shell is limited.\x1b[0m`);
      } else {
        term.writeln(`\x1b[36mConnected to ${deviceName || serial}. Tip: "adb ..." routes to ADB server.\x1b[0m`);
      }
    } catch (err) {
      term.writeln(`\r\n\x1b[31m[Failed to open shell: ${err}]\x1b[0m`);
      currentSessionId = null;
    }
  }

  async function closeSession() {
    if (currentSessionId) {
      await invoke<void>("shell_close", { sessionId: currentSessionId }).catch(() => {});
      currentSessionId = null;
    }
  }

  onMount(() => {
    let currentTheme = "dark";
    let currentColorPreset: TerminalColorPreset = "default";

    const unsubTheme = themeStore.subscribe((t) => {
      currentTheme = t;
      if (term) {
        term.options.theme = getTermTheme(t, currentColorPreset);
      }
    });

    const unsubColor = terminalColorStore.subscribe((preset) => {
      currentColorPreset = preset;
      if (term) {
        term.options.theme = getTermTheme(currentTheme, preset);
      }
    });

    term = new Terminal({
      fontFamily: '"Cascadia Code", "Fira Code", "Consolas", monospace',
      fontSize: 12,
      lineHeight: 1.4,
      theme: getTermTheme(currentTheme, currentColorPreset),
      cursorBlink: true,
      cursorStyle: "block",
      scrollback: 5000,
      rightClickSelectsWord: true,
    });

    // Ctrl+C: copy if selection exists, otherwise send interrupt
    term.attachCustomKeyEventHandler((e: KeyboardEvent) => {
      if (e.key === "c" && e.ctrlKey && !e.shiftKey && !e.altKey) {
        if (term && term.hasSelection()) {
          navigator.clipboard.writeText(term.getSelection());
          term.clearSelection();
          return false; // prevent xterm from processing
        }
        // No selection — let it pass through as \x03 (interrupt)
      }
      if (e.key === "v" && e.ctrlKey && !e.shiftKey && !e.altKey) {
        // Allow Ctrl+V paste to work via browser
        return false;
      }
      return true;
    });

    fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    term.open(termContainer);
    fitAddon.fit();

    resizeObserver = new ResizeObserver(() => {
      if (resizeTimeout !== null) {
        window.clearTimeout(resizeTimeout);
      }
      resizeTimeout = window.setTimeout(() => {
        resizeTimeout = null;
        fitAddon?.fit();
        // Forward terminal dimensions to backend for Shell V2 PTY resize
        if (currentSessionId && term) {
          invoke<void>("shell_resize", {
            sessionId: currentSessionId,
            rows: term.rows,
            cols: term.cols,
          }).catch(() => {}); // Best effort — ignore errors
        }
      }, 150);
    });
    resizeObserver.observe(termContainer);

    // Forward keystrokes with adb prefix interception
    term.onData((data: string) => {
      if (!currentSessionId || !term) return;

      // Check for Enter
      if (data === "\r" || data === "\n" || data === "\r\n") {
        const trimmed = lineBuffer.trim();
        if (trimmed.toLowerCase() === "adb" || trimmed.toLowerCase().startsWith("adb ")) {
          // Intercept: route to local ADB command
          const adbArgs = trimmed.toLowerCase() === "adb" ? "" : trimmed.slice(4).trim();
          term.write("\r\n");
          lineBuffer = "";

          if (!adbArgs) {
            term.writeln("Supported: adb devices, adb shell <cmd>");
            return;
          }

          invoke<string>("adb_local_command", { args: adbArgs })
            .then((output) => {
              if (term) {
                term.write(output.replace(/\n/g, "\r\n"));
              }
            })
            .catch((err) => {
              if (term) {
                term.writeln(`\x1b[31m${err}\x1b[0m`);
              }
            });
          return; // Don't send Enter to device shell
        }
        lineBuffer = "";
      } else if (data === "\x7f" || data === "\b") {
        // Backspace
        lineBuffer = lineBuffer.slice(0, -1);
      } else if (data.length === 1 && data >= " ") {
        // Printable character
        lineBuffer += data;
      } else if (data.length > 1) {
        // Paste: add all printable characters to buffer
        for (const ch of data) {
          if (ch >= " " && ch !== "\x7f") {
            lineBuffer += ch;
          }
        }
      }

      // Forward to device shell
      const bytes = Array.from(new TextEncoder().encode(data));
      invoke<void>("shell_write", { sessionId: currentSessionId, data: bytes }).catch(
        (err) => {
          if (term) {
            term.writeln(`\r\n\x1b[31m[Write error: ${err}]\x1b[0m`);
          }
        }
      );
    });

    return () => {
      if (resizeTimeout !== null) {
        window.clearTimeout(resizeTimeout);
        resizeTimeout = null;
      }
      unsubTheme();
      unsubColor();
      closeSession();
      resizeObserver?.disconnect();
      term?.dispose();
      term = null;
    };
  });

  function copyTerminal() {
    if (term) {
      const selection = term.getSelection();
      if (selection) {
        navigator.clipboard.writeText(selection);
        term.clearSelection();
      }
    }
  }

  function clearTerminal() {
    if (term) {
      term.clear();
    }
  }

  async function startLogcat() {
    if (!term || isLogcatActive) return;

    const sessionId = `logcat-${crypto.randomUUID()}`;
    logcatSessionId = sessionId;
    isLogcatActive = true;

    term.writeln(`\r\n\x1b[36m[Logcat started — level=${logcatLevel}]\x1b[0m`);

    const output = new Channel<ShellOutput>();
    output.onmessage = (msg: ShellOutput) => {
      if (!term) return;
      if (msg.kind === "Data") {
        term.write(new Uint8Array(msg.data));
      } else if (msg.kind === "Stderr") {
        term.write(new Uint8Array(msg.data));
      } else if (msg.kind === "Exit") {
        const codeStr = msg.code !== undefined ? ` (exit ${msg.code})` : "";
        term.writeln(`\r\n\x1b[90m[${msg.message}${codeStr}]\x1b[0m`);
        isLogcatActive = false;
        logcatSessionId = "";
      }
    };

    try {
      await invoke<void>("logcat_start", {
        sessionId,
        onOutput: output,
        level: logcatLevel,
        tag: null,
        pid: null,
      });
      logStore.add(`Logcat started: level=${logcatLevel}`);
    } catch (err) {
      term.writeln(`\r\n\x1b[31m[Failed to start logcat: ${err}]\x1b[0m`);
      isLogcatActive = false;
      logcatSessionId = "";
    }
  }

  async function stopLogcat() {
    if (!logcatSessionId) return;

    try {
      await invoke<void>("logcat_stop", { sessionId: logcatSessionId });
    } catch {
      // Best effort
    }

    if (term) {
      term.writeln(`\r\n\x1b[33m[Logcat stopped]\x1b[0m`);
    }
    isLogcatActive = false;
    logcatSessionId = "";
  }

  // React to device changes — wait for deviceReady to avoid USB race with queryDeviceInfo
  $effect(() => {
    const device = $currentDevice;
    const ready = $deviceReady;
    if (device?.protocol === "Adb" && device.serial && ready && term) {
      openSession(device.serial, device.product || null, device.adb_state || null);
    } else if (term && !ready) {
      // Still loading — don't show disconnect message
    } else if (term) {
      stopLogcat();
      if (currentSessionId) {
        closeSession();
        term.writeln("\x1b[90mDevice disconnected.\x1b[0m");
      } else {
        term.writeln("\x1b[90mConnect an ADB device to use the terminal.\x1b[0m");
      }
    }
  });
</script>

<div class="terminal-wrapper">
  <div class="terminal-actions">
    <select
      class="color-select"
      value={$terminalColorStore}
      onchange={(e) => terminalColorStore.set((e.target as HTMLSelectElement).value as TerminalColorPreset)}
    >
      {#each terminalColorPresets as preset}
        <option value={preset.id}>{preset.label}</option>
      {/each}
    </select>
    <select
      class="logcat-level"
      bind:value={logcatLevel}
      disabled={isLogcatActive || $currentDevice?.protocol !== "Adb"}
    >
      <option value="V">V</option>
      <option value="D">D</option>
      <option value="I">I</option>
      <option value="W">W</option>
      <option value="E">E</option>
      <option value="F">F</option>
    </select>
    {#if isLogcatActive}
      <button class="action-btn action-btn-warn" onclick={stopLogcat}>Stop Logcat</button>
    {:else}
      <button class="action-btn" onclick={startLogcat} disabled={$currentDevice?.protocol !== "Adb"}>Logcat</button>
    {/if}
    <button class="action-btn" onclick={copyTerminal}>Copy</button>
    <button class="action-btn" onclick={clearTerminal}>Clear</button>
  </div>
  <div class="shell-container" bind:this={termContainer}></div>
</div>

<style>
  .terminal-wrapper {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }

  .terminal-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    padding: 4px 12px;
  }

  .color-select {
    font-size: var(--font-sm);
    font-family: inherit;
    padding: 2px 6px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--surface);
    color: var(--text-muted);
    cursor: pointer;
    transition: border-color 0.15s;
  }

  .color-select:hover {
    border-color: var(--primary);
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

  .action-btn:hover:not(:disabled) {
    background: var(--surface-hover);
  }

  .action-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .action-btn-warn {
    color: var(--warning, #f39c12);
  }

  .logcat-level {
    background: var(--input-bg, var(--surface));
    border: 1px solid var(--border-strong, var(--border));
    border-radius: 6px;
    padding: 2px 6px;
    font-size: var(--font-xs, 11px);
    font-family: "Cascadia Code", "Fira Code", "Consolas", monospace;
    color: var(--text);
    cursor: pointer;
    transition: border-color 0.15s;
  }

  .logcat-level:hover {
    border-color: var(--primary);
  }

  .logcat-level:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .shell-container {
    background: var(--log-bg);
    flex: 1;
    min-height: 0;
    padding: 4px;
    overflow: hidden;
  }

  /* Override xterm.js viewport to match our design */
  .shell-container :global(.xterm-viewport) {
    overflow-y: auto !important;
  }

  .shell-container :global(.xterm) {
    padding: 4px 8px;
  }
</style>
