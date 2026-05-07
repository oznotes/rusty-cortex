import { writable } from "svelte/store";
import type { LogEntry } from "../types";

function createLogStore() {
  const { subscribe, update } = writable<LogEntry[]>([]);

  return {
    subscribe,
    add(message: string, level: LogEntry["level"] = "info") {
      update((entries) => [
        ...entries,
        { timestamp: new Date(), message, level },
      ]);
    },
    clear() {
      update(() => []);
    },
  };
}

export const logStore = createLogStore();
