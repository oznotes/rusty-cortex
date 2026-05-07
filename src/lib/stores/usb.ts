import { writable } from 'svelte/store';
import { invoke } from '@tauri-apps/api/core';

const stored = typeof localStorage !== 'undefined'
  ? localStorage.getItem('usb-direct-mode') === 'true'
  : false;

export const usbDirectMode = writable(stored);

// Sync to backend on every change
usbDirectMode.subscribe(async (v) => {
  if (typeof localStorage !== 'undefined') {
    localStorage.setItem('usb-direct-mode', String(v));
  }
  try {
    await invoke<void>('set_usb_mode', { forceUsb: v });
  } catch {
    // Backend not ready yet during init
  }
});
