# Rusty Cortex

Multi-protocol mobile phone flashing tool. Built with Tauri v2 + Svelte 5. **v0.8.8**

## What it does

Flash, backup, install, and interact with Android phones via:
- **Fastboot** — flash, erase, getvar, reboot, dynamic partition list
- **ADB** — sideload, push/pull, install APK, partition dump, interactive shell with smart routing, reboot modes, getprop, live logcat streaming (binary format parsing, priority/tag/PID filtering, ANSI color output)

- **EDL (Qualcomm)** — Sahara device identification, **programmer auto-detection database** (learns from connections), programmer folder scanner with **binary identity parser** (ELF/MBN certificate chain extraction → HWID/PKHash matching with cryptographic certainty), **hash filter** with 5-tier scoring (DbExact > BinaryVerified > FilenameMatch > Unknown > DbOtherDevice), **programmer detail view** (chipset, HWID, OEM, PKHash, match status), Firehose partition listing/read/dump, **partition write (sparse-aware) with SHA256 verification**, partition erase, rawprogram.xml + patch.xml batch flash, Firehose-mode recovery, reboot (Tier 3)

**Sahara stale session recovery** — 3-strategy identify chain: PblHack (blind HELLO_RSP, same transport) → RESET_REQ (USB re-enum) → Firehose nop probe. Superior to bkerler/edl which has no recovery.

MTK BROM is planned for a later phase.

## Tech Stack

- **Backend:** Rust, Tauri v2, nusb 0.1, fastboot-protocol, adb_client 3.x
- **Frontend:** Svelte 5 (runes), TypeScript, @xterm/xterm
- **Platform:** Windows first

## Backend Capabilities

### Fastboot
- Flash and erase partitions
- Read device variables (getvar)
- Reboot: Normal, Bootloader, Recovery
- Dynamic partition list

### ADB (Direct USB + server fallback)
- **Direct USB mode**: nusb bulk transfers, no ADB server needed — **fully standalone for ALL operations including interactive shell**
- **RSA AUTH handshake**: PKCS1v15+SHA1 prehash signing with ~/.android/adbkey
- Device detection: USB class-based scan (0xFF/0x42/0x01) + ADB server fallback with state awareness (Normal/Recovery/Sideload)
- Recovery mode: graceful handling — skip getprop, adapt UI, limit actions
- getprop (800+ properties)
- Reboot: Normal, Bootloader, Recovery, EDL
- Sideload: raw sideload-host protocol for ZIP transfer
- Push/Pull: raw SYNC protocol with atomic .tmp write
- Install APK: push to temp + pm install with flags (-r, -d, -g) + cleanup
- **Shell V2 protocol**: Binary-safe streaming, terminal colors, separate stderr, exit codes
- Shell V2 feature detection: `host-serial:{serial}:features` with per-device cache
- Interactive shell: Shell V2 with `TERM=xterm-256color` (PTY mode) or V1 fallback, adb prefix routing
- Partition dump: **V2: direct streaming (no temp file, zero device storage)** / V1: dd to temp + SYNC pull + DeviceTempGuard cleanup
- Raw image dump: **V2: direct streaming** / V1: dd with offset/size + SYNC pull + cleanup
- **Dump resume**: skips already-dumped partitions by comparing local file size vs device partition size
- **Disconnect detection**: immediate USB disconnect detection + TCP timeout, clear error with bytes received
- Root detection: auto-detects adbd root vs su
- Batch partition listing: single shell for-loop (1 TCP vs 100+)
- Writable temp path detection: /data/local/tmp → /sdcard/ → /tmp/
- Device space check via df
- **Live logcat**: binary format parsing, priority/tag/PID filtering, ANSI color output, streaming via Channel

### Safety
- FlashGuard RAII: concurrency guard that resets on panic/cancellation
- adb_shell timeout: 30s for short commands, configurable for long ops
- Atomic file writes: .tmp rename prevents partial files on disconnect
- DeviceTempGuard: RAII cleanup of device temp files on error
- Overwrite confirmation: checks existing files before dump
- APK filename validation: prevents shell injection
- Shell path validation: `validate_shell_path()` rejects metacharacters on all user-controlled paths
- Flash timeout: 10-minute safety timeout on fastboot flash operations

## UI

Sidebar + workspace layout with dark (default) and light themes.

- **Sidebar:** Device info, mode badge (orange for Recovery/Sideload), reboot buttons, detect
- **Workspace:** Tabbed for ADB — Sideload | File Transfer | Dump. Fastboot flash controls.
- **File Transfer:** Compact horizontal action rows — Push, Pull, Install APK inline
- **Dump Controls:** Read/Write inner tabs, collapsible partition list, image read, partition write with confirmation
- **Bottom panel:** Resizable with drag handle, minimize-to-tab-bar. Log | Terminal tabs.
- **Terminal:** Interactive xterm.js shell with Shell V2 colors, stderr display, exit codes, smart `adb` prefix routing
- **Progress bar:** Dual-mode — determinate percentage for ADB ops, indeterminate for Fastboot

Design system documented in `docs/specs/design-dna.md`.

## Project Structure

```
src-tauri/src/
  lib.rs              App setup, command registration, logging
  main.rs             Entry point
  commands.rs         Tauri IPC command handlers
  types.rs            DeviceInfo, FlashProgress, AdbState, RootType, PartitionInfo, ProgrammerIdentity, DeviceHealth
  error.rs            FlashError enum
  device/
    detect.rs         USB device scanning (nusb + interface descriptor) + ADB server query
  flash/
    manager.rs        Flash workflow orchestration
    validation.rs     Pre-flash safety checks
  protocols/
    mod.rs            FlashProtocol trait
    fastboot.rs       Fastboot protocol (interface descriptor detection)
    adb_usb.rs        ADB USB transport (message dispatcher, CNXN/AUTH, UsbStream, connection cache)
    adb/              ADB protocol — split into focused modules:
      mod.rs          Core transport, device detection, FlashProtocol impl
      shell.rs        Shell V1/V2, feature detection, interactive shell, stdin streaming
      dump.rs         Partition dump, raw image dump, partition write, DeviceTempGuard
      sync.rs         SYNC protocol (push/pull)
      sideload.rs     Sideload-host protocol
      install.rs      APK installation
      logcat.rs       Binary logcat parser, priority/tag/PID filtering, ANSI color output
    edl.rs            EDL protocol: Sahara identify/connect, Firehose operations, Xiaomi EDL auth
    edl_gpt.rs        GPT partition table parser (UEFI format_guid, partition categories)
    edl_usb.rs        EDL USB transport (nusb 0.1 bulk I/O, qdlrs QdlReadWrite)
    edl_mbn.rs        Qualcomm binary parser (ELF/MBN identity, DER cert chain, chipset lookup, shared magic validation)
    sparse.rs         Android sparse image format parser/decompressor
    edl_xml.rs        rawprogram.xml + patch.xml parser (quick-xml 0.37)
    edl_db.rs         Programmer database (JSON persistence, folder scanner, hash filter + binary scoring)

src/
  App.svelte          Root layout, dynamic version badge
  lib/
    components/       Sidebar, Workspace, FlashControls, AdbControls, FileTransfer,
                      DumpControls, PartitionRead, PartitionWrite, DeviceBrowser,
                      EdlControls, EdlPartitionRead, EdlPartitionWrite,
                      BottomPanel, ShellTerminal, ProgressBar, LogPanel, ThemeToggle
    stores/           device, flash, log, theme, terminalColor, usb
    types.ts          TypeScript types
```

## Setup

```bash
# Prerequisites: Rust toolchain, Node.js 18+
npm install
npm run tauri dev
```

## Build

```bash
npm run tauri build
# Output: src-tauri/target/release/bundle/nsis/ and /msi/
```

## Tested Devices

| Device | Protocol | USB Direct | Notes |
|---|---|---|---|
| Xiaomi Mi 9T Pro (raphael) | Fastboot + ADB | Yes | Full feature coverage |
| Xiaomi 11 Lite 5G NE (lisa) | Fastboot + ADB | Yes | 966 properties, 6+ consecutive detects |
| Xiaomi Redmi K20 Pro (raphael) | ADB + EDL | Yes | 803 properties, EDL auth ACKed, UFS 8 LUNs |
| Motorola moto g 2025 | ADB | Yes | 1507 properties, device switching works |
| Samsung Galaxy S20 (x1q) | ADB | Yes | 1119 properties, 79 partitions, Samsung driver solved |
| Samsung (kona) | Fastboot | — | Detect + vars only (needs Odin for flash) |

## Status

Phase 1 (Fastboot) and Phase 2 (ADB) complete. Shell V2 protocol shipped (v0.5.0). Disconnect detection instant (v0.8.0+). ADB Direct USB mode shipped (v0.7.0). **USB message dispatcher shipped (v0.8.0)** — AOSP-style single reader thread with channel routing. Samsung USB Direct solved via vendored nusb with composite driver patch. Type-safe IPC (enum deserialization). Terminal color presets (4 themes). Fastboot real percentage progress. **Reliable on all 4 USB Direct test devices.** CSS design system with RGB variable triplets for theme-aware alpha colors. **DumpControls split into 3 components with Read/Write tabs (v0.8.4).** **EDL Tier 3: programmer auto-detection database + post-write SHA256 verification.** **Programmer Intelligence Phase 2: binary parser extracts HWID/PKHash from ELF/MBN certificate chains — cryptographic programmer-device matching.** **GitHub Desktop-style header: always-dark native + in-app status bar with live device info.** **Compact EDL workspace: single-line controls, clickable inputs, storage badges, programmer detail view.** **Sahara PblHack stale recovery: 3-strategy identify chain, blind HELLO_RSP on same transport.** **PblHack stale recovery verified on K20 Pro.** **EDL architecture review (Session 40): clippy fixes, generic attr parser, HashMap ref serialize, Sahara identity helper, GPT expect, header brightness.** **EDL code quality polish (Session 41): deduplicated magic constants + programmer validation across edl_mbn/edl_db/edl, extracted GPT parser to edl_gpt.rs, edl.rs 2354→2053 lines.** 269 Rust tests, 0 clippy warnings, 0 svelte-check errors. Release builds (MSI + NSIS) verified.
