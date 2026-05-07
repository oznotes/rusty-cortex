import { writable } from "svelte/store";
import { listen } from "@tauri-apps/api/event";
import type { FlashStage, FlashProgress } from "../types";
import { logStore } from "./log";

export const flashStage = writable<FlashStage>("Idle");
export const flashMessage = writable("");
export const flashPercent = writable<number | null>(null);
export const isFlashing = writable(false);
export const firmwarePath = writable("");
export const selectedPartition = writable("boot");

// Set up flash-progress listener lazily (only once, when first flash starts)
let listenerReady = false;
export function ensureFlashListener() {
  if (listenerReady) return;
  listenerReady = true;
  listen<FlashProgress>("flash-progress", (event) => {
    flashStage.set(event.payload.stage);
    flashMessage.set(event.payload.message);
    flashPercent.set(event.payload.percent ?? null);
    // Only log stage transitions — progress bar handles real-time visual feedback
    const stage = event.payload.stage;
    if (stage === "Complete" || stage === "Error" || stage === "Validating") {
      logStore.add(event.payload.message);
    }
  });
}
