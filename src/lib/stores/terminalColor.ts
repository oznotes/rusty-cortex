import { writable, get } from "svelte/store";

export type TerminalColorPreset = "default" | "classic-green" | "retro-amber" | "white-on-dark";

export const terminalColorPresets: { id: TerminalColorPreset; label: string }[] = [
  { id: "default", label: "Default" },
  { id: "classic-green", label: "Classic Green" },
  { id: "retro-amber", label: "Retro Amber" },
  { id: "white-on-dark", label: "White on Dark" },
];

// xterm ITheme objects for each preset (independent of app theme)
const presetThemes: Record<Exclude<TerminalColorPreset, "default">, Record<string, string>> = {
  "classic-green": {
    background: "#0a0a0a",
    foreground: "#33ff33",
    cursor: "#33ff33",
    cursorAccent: "#0a0a0a",
    selectionBackground: "rgba(51, 255, 51, 0.2)",
    black: "#0a0a0a",
    red: "#ff3333",
    green: "#33ff33",
    yellow: "#ffff33",
    blue: "#3399ff",
    magenta: "#ff33ff",
    cyan: "#33ffff",
    white: "#33ff33",
  },
  "retro-amber": {
    background: "#0c0800",
    foreground: "#ffb000",
    cursor: "#ffb000",
    cursorAccent: "#0c0800",
    selectionBackground: "rgba(255, 176, 0, 0.2)",
    black: "#0c0800",
    red: "#ff4444",
    green: "#ffb000",
    yellow: "#ffdd44",
    blue: "#aa8800",
    magenta: "#ff8844",
    cyan: "#ffcc44",
    white: "#ffb000",
  },
  "white-on-dark": {
    background: "#000000",
    foreground: "#f0f0f0",
    cursor: "#f0f0f0",
    cursorAccent: "#000000",
    selectionBackground: "rgba(240, 240, 240, 0.2)",
    black: "#000000",
    red: "#ff5555",
    green: "#55ff55",
    yellow: "#ffff55",
    blue: "#5555ff",
    magenta: "#ff55ff",
    cyan: "#55ffff",
    white: "#f0f0f0",
  },
};

function getInitialPreset(): TerminalColorPreset {
  if (typeof localStorage !== "undefined") {
    const stored = localStorage.getItem("terminalColor");
    if (stored && terminalColorPresets.some((p) => p.id === stored)) {
      return stored as TerminalColorPreset;
    }
  }
  return "default";
}

function createTerminalColorStore() {
  const store = writable<TerminalColorPreset>(getInitialPreset());

  return {
    subscribe: store.subscribe,
    set(preset: TerminalColorPreset) {
      localStorage.setItem("terminalColor", preset);
      store.set(preset);
    },
  };
}

export const terminalColorStore = createTerminalColorStore();

/** Get the xterm theme for a preset. "default" returns null (caller uses app theme). */
export function getPresetTheme(preset: TerminalColorPreset): Record<string, string> | null {
  if (preset === "default") return null;
  return presetThemes[preset];
}
