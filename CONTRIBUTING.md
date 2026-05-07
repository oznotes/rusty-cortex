# Contributing to Rusty Cortex

This repository is **Rusty Cortex**, a multi-protocol mobile phone flashing tool built with Rust (Tauri v2) and Svelte 5.

**Owner:** Ozgur Oz

---

## Quality Standard

Write code like you're submitting a patch to Linus Torvalds. Every line must justify its existence.

- No dead code, no commented-out code, no TODO-later hacks left in production paths
- No band-aids â€” find the root cause and fix it properly
- No unnecessary abstractions â€” three similar lines beat a premature helper
- If it compiles with warnings, it's not done
- If it panics in production, you broke it â€” handle errors explicitly
- Test before you claim it works

## Architecture

```
src-tauri/          # Rust backend (Tauri v2)
  src/
    lib.rs          # App setup, command registration, logging
    commands.rs     # Tauri IPC command handlers (31 commands)
    types.rs        # DeviceInfo, FlashProgress, ProtocolType, AdbState, RebootMode, RootType, RootStatus, PartitionInfo
    error.rs        # FlashError enum
    device/
      detect.rs     # USB device scanning (nusb + VID/PID table) + ADB server query
    flash/
      manager.rs    # Flash workflow orchestration
      validation.rs # Pre-flash safety checks
    protocols/
      mod.rs        # Protocol module exports
      fastboot.rs   # Fastboot protocol (detect, flash, getvar, reboot)
      adb/          # ADB protocol (split into focused modules)
        mod.rs      # Core transport, device detection, reboot, getprop
        shell.rs    # Shell V1/V2, feature detection, interactive shell
        dump.rs     # Partition dump, raw image dump
        sync.rs     # SYNC protocol (push/pull)
        sideload.rs # Sideload-host protocol
        install.rs  # APK installation
        logcat.rs   # Binary logcat parser, priority/tag/PID filtering, ANSI color output
      edl.rs        # EDL protocol: Sahara identify/connect, Firehose operations, Xiaomi EDL auth (sig + RSA token)
      edl_gpt.rs    # GPT partition table parser (UEFI format_guid, partition categories, header/entry parsing)
      edl_usb.rs    # EDL USB transport (nusb 0.1 bulk I/O, qdlrs QdlReadWrite)
      sparse.rs     # Android sparse image format parser/decompressor
      edl_xml.rs    # rawprogram.xml + patch.xml parser (quick-xml 0.37)
      edl_db.rs     # Programmer database (JSON persistence, auto-detect, folder scanner, hash filter scoring)

src/                # Svelte 5 frontend (TypeScript)
  App.svelte        # Root layout (title bar, sidebar, workspace, bottom panel)
  lib/
    components/     # UI components (Sidebar, Workspace, FlashControls, AdbControls, FileTransfer, DumpControls, PartitionRead, PartitionWrite, EdlControls, EdlPartitionRead, EdlPartitionWrite, BottomPanel, ShellTerminal, ProgressBar, LogPanel)
    stores/         # Svelte stores (device, flash, log, theme)
    types.ts        # TypeScript types mirroring Rust types
```

## Tech Stack

| Layer | Technology |
|-------|-----------|
| App Framework | Tauri v2 |
| Backend | Rust |
| Frontend | Svelte 5 (runes mode) + TypeScript |
| USB | nusb 0.1 (pure Rust) |
| Fastboot | fastboot-protocol |
| ADB | adb_client 3.x + raw Shell V1 / SYNC over TCP |
| Terminal | @xterm/xterm |
| Logging | tracing + tracing-subscriber |

## Rules

### Rust Backend
- All Tauri commands that do I/O must be `async`
- **Never block the Tokio async runtime** â€” use `tokio::task::spawn_blocking()` for blocking operations (USB enumeration, file I/O)
- Always add timeouts to external operations (USB, protocol commands)
- Use `tracing::info!` / `tracing::error!` for logging, never `println!`
- Errors flow through `FlashError` enum â†’ serialized to frontend via `Result<T, String>`
- `unwrap()` is banned in command handlers â€” use `map_err` or `?`

### Svelte Frontend
- **Svelte 5 runes only** â€” use `$state`, `$derived`, `$effect`, not legacy `$:` or `let` reactivity
- Use `onclick={handler}` not `on:click={handler}` (Svelte 5 event syntax)
- Store subscriptions use `$store` syntax
- **Never call `listen()` from `@tauri-apps/api/event` inside `onMount`** â€” it creates IPC connections that block subsequent `invoke()` calls on Windows/WebView2. Set up event listeners lazily or at store level.
- Keep components thin â€” business logic goes in stores or utility functions
- Type all invoke calls: `invoke<ReturnType>("command_name", { args })`

### IPC
- Frontend â†” Backend communication via `invoke()` only
- Events (backend â†’ frontend) via Tauri events for real-time updates (progress, logs)
- Event listeners must be set up lazily, not on component mount

### Known Pitfalls
- `nusb::list_devices().wait()` is synchronous and can hang on Windows â€” always wrap in `spawn_blocking` + timeout
- Tauri v2 on WebView2 has IPC connection limits â€” avoid long-lived event subscriptions during startup
- NSIS installer fails if the app exe is still running â€” close before rebuilding

## Project Status

- **Phase 1 (Fastboot):** Complete â€” detection, flash, erase, getvar, reboot, partition list
- **Phase 2 (ADB):** Complete â€” detection, getprop, reboot, sideload, push/pull, install APK, shell (smart routing), partition dump, raw image dump, device file browser
- **Phase 2.5 (ADB UX):** Complete â€” horizontal action rows, resizable panel, edge case hardening, recovery mode detection
- **Phase 3 (Shell V2 + USB Direct):** Complete â€” Shell V2 binary streaming, dump resume, ADB Direct USB with AOSP-style message dispatcher (v0.8.0)
- **Phase 3.5:** Partition write, Dump UI redesign, USB interactive shell, terminal colors â€” **COMPLETE**
- **Phase 3.6:** Fastboot fix, reboot polish, architecture review, FlashProtocol trait removal â€” **COMPLETE**
- **Phase 4:** EDL Tiers 1-3 complete + auth + hash filter â€” **EDL (Qualcomm)** shipped with programmer auto-detection database + post-write verification + Xiaomi EDL authentication (`sig` + RSA token, ACKed on K20 Pro) + **hash filter** (score scan results by HWID/PKHash). Live logcat streaming shipped. MTK BROM, Magisk auto-root planned.

## Design Docs

- Design DNA: `docs/specs/design-dna.md`
- ADB Dump spec: `docs/superpowers/specs/2026-03-28-adb-dump-design.md`
- ADB Dump plan: `docs/superpowers/plans/2026-03-28-adb-dump.md`
- Phase 1 spec: `docs/superpowers/specs/2026-03-26-multi-flash-tool-design.md`
- UI Redesign spec: `docs/superpowers/specs/2026-03-26-ui-redesign-design.md`
- Recovery mode spec: `docs/superpowers/specs/2026-03-28-recovery-mode-design.md`
