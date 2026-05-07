#!/usr/bin/env bash
# Nexus 6P Linux ADB diagnostic — run while device is plugged in (~2 min)
# Captures three independent signals so we can pinpoint the failure.
#
# After running, share the three output files:
#   /tmp/nexus6p-stockadb.txt
#   /tmp/nexus6p-lsusb.txt
#   ~/.local/share/rusty-cortex/logs/rusty-cortex.log.<today>

set -u
SERIAL="84B7N15A07006268"
OUT_STOCK="/tmp/nexus6p-stockadb.txt"
OUT_LSUSB="/tmp/nexus6p-lsusb.txt"

echo "=== 1/3  Stock adb sanity check  ==="
echo "Tells us whether the device responds to *any* host on this Linux box."
echo "If this hangs or fails too -> device or kernel issue, not our client."
echo
{
  echo "### date"; date
  echo "### adb version"; adb version 2>&1
  echo "### adb devices"; timeout 10 adb devices -l 2>&1
  echo "### adb -s $SERIAL shell getprop ro.build.version.release"
  timeout 10 adb -s "$SERIAL" shell getprop ro.build.version.release 2>&1
  echo "### adb -s $SERIAL shell getprop ro.build.version.sdk"
  timeout 10 adb -s "$SERIAL" shell getprop ro.build.version.sdk 2>&1
  echo "### adb -s $SERIAL shell getprop ro.product.model"
  timeout 10 adb -s "$SERIAL" shell getprop ro.product.model 2>&1
} | tee "$OUT_STOCK"
echo
echo "Saved -> $OUT_STOCK"
echo

echo "=== 2/3  USB descriptor + driver binding  ==="
echo "Tells us what kernel driver (if any) is bound to interface 0."
echo
{
  echo "### lsusb -v for 18d1:4ee7"
  lsusb -d 18d1:4ee7 -v 2>&1 | head -120
  echo
  echo "### sysfs driver binding"
  for d in /sys/bus/usb/devices/*; do
    if [ -f "$d/idVendor" ] && [ "$(cat "$d/idVendor" 2>/dev/null)" = "18d1" ]; then
      echo "--- $d ---"
      ls -la "$d"/*/driver 2>/dev/null
    fi
  done
} | tee "$OUT_LSUSB"
echo
echo "Saved -> $OUT_LSUSB"
echo

echo "=== 3/3  Rusty-cortex with debug logging  ==="
echo "Now: kill any running rusty-cortex, then launch from THIS terminal with:"
echo
echo "    pkill -f rusty-cortex; sleep 1; \\"
echo "    adb kill-server; sleep 1; \\"
echo "    RUST_LOG='info,rusty_cortex_lib::protocols::adb_usb=debug,rusty_cortex_lib::protocols::adb=debug' \\"
echo "    /home/oz/Github/Repos/rusty/src-tauri/target/release/rusty-cortex"
echo
echo "In the GUI: pick the Nexus 6P from the Devices list (same flow as today)."
echo "Wait until 'Device variable query timed out' fires once, then quit."
echo
echo "The new log will be at:"
echo "    ~/.local/share/rusty-cortex/logs/rusty-cortex.log.\$(date +%F)"
echo
echo "Look for these debug lines that aren't in today's log:"
echo "  - 'Stale data in USB endpoint (bad magic)'   -> device IS responding, we reject it"
echo "  - 'Discarding stale ADB message'              -> device sending old session data"
echo "  - (silence after 'Sending ADB CNXN')          -> device sending nothing at all"
echo
echo "Done."
