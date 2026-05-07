import { writable } from "svelte/store";
import type { DeviceInfo, DeviceHealth, EdlDeviceInfo, EdlPartitionEntry } from "../types";

export const currentDevice = writable<DeviceInfo | null>(null);
export const isDetecting = writable(false);
export const deviceVars = writable<Record<string, string>>({});
export const devicePartitions = writable<string[]>([]);
export const deviceHealth = writable<DeviceHealth | null>(null);
/** Set after device vars are queried — shell waits for this to avoid USB race. */
export const deviceReady = writable(false);

// EDL (Qualcomm Emergency Download) stores
export const edlInfo = writable<EdlDeviceInfo | null>(null);
export const edlPartitions = writable<EdlPartitionEntry[]>([]);
export const edlConnected = writable(false);
