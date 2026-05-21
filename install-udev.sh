#!/usr/bin/env bash
# Install udev rules for the Thermaltake TH420 V2 LCD (one-time setup, needs sudo).
set -euo pipefail

RULES_DST="/etc/udev/rules.d/99-th420.rules"
RULES_CONTENT='SUBSYSTEM=="hidraw", ATTRS{idVendor}=="264a", ATTRS{idProduct}=="233c", MODE="0660", TAG+="uaccess"'

if [ "$(id -u)" -ne 0 ]; then
    echo "Requesting sudo to install udev rules..."
    exec sudo "$0" "$@"
fi

echo "$RULES_CONTENT" > "$RULES_DST"
udevadm control --reload-rules
udevadm trigger
echo "Installed $RULES_DST"
echo "If the device is already plugged in, unplug and replug it."
