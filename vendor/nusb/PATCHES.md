# nusb 0.1.14 — Local Patches

**Upstream:** https://github.com/kevinmehall/nusb (v0.1.14)
**Vendored:** 2026-04-02 (Rusty Cortex Session 19)

## Patch: Generalized Composite Parent Driver Check

**File:** `src/platform/windows_winusb/enumeration.rs`

**Problem:** nusb only recognizes `usbccgp` and `winusb` as parent device drivers on Windows. Samsung devices use `dg_ssudbus` as their composite parent — nusb rejects them even though the ADB child interface has WinUSB installed.

**Fix:** Two changes:

1. **`probe_device()` line 73:** Changed from `if driver == "usbccgp"` to `if driver != "winusb"`. Walks children for any composite parent, not just usbccgp. Safe because `children()` returns empty for non-composite devices.

2. **`find_device_interface_path()` lines 133-196:** Restructured from three-way branch (usbccgp/winusb/error) to two-way (winusb/else-composite). The child-level WinUSB check is preserved — only opens interfaces whose driver is WinUSB.

**How to re-apply after re-vendoring:**
1. Copy fresh nusb source to `vendor/nusb/`
2. In `src/platform/windows_winusb/enumeration.rs`:
   - Line 73: change `usbccgp` check to `!winusb` check
   - Lines 133+: swap winusb branch first, else composite path
3. Preserve the child-level WinUSB guard in the composite path

**Research:** `docs/research/2026-04-02-samsung-winusb-research.md`
