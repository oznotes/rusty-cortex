import { writable, get } from "svelte/store";
import { getCurrentWindow } from "@tauri-apps/api/window";

type Theme = "dark" | "light";

function getInitialTheme(): Theme {
  if (typeof localStorage !== "undefined") {
    const stored = localStorage.getItem("theme");
    if (stored === "dark" || stored === "light") return stored;
  }
  return "dark";
}

function applyTheme(theme: Theme) {
  document.documentElement.setAttribute("data-theme", theme);
  localStorage.setItem("theme", theme);
  getCurrentWindow().setTheme(theme === "dark" ? "dark" : "light");
}

function createThemeStore() {
  const store = writable<Theme>(getInitialTheme());

  return {
    subscribe: store.subscribe,
    toggle() {
      const current = get(store);
      const next: Theme = current === "dark" ? "light" : "dark";
      applyTheme(next);
      store.set(next);
    },
    init() {
      const theme = getInitialTheme();
      applyTheme(theme);
      store.set(theme);
    },
  };
}

export const themeStore = createThemeStore();
