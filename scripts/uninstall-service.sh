#!/usr/bin/env bash
set -euo pipefail

# KataGo WebUI - systemd uninstaller (system-wide)
# Usage: sudo ./scripts/uninstall-service.sh

UNIT_NAME="katago-webui.service"
UNIT_PATH="/etc/systemd/system/${UNIT_NAME}"

echo "Stopping and disabling service ..."
systemctl disable --now "$UNIT_NAME" || true

if [[ -f "$UNIT_PATH" ]]; then
  echo "Removing unit file $UNIT_PATH ..."
  rm -f "$UNIT_PATH"
fi

echo "Reloading systemd ..."
systemctl daemon-reload

echo "Uninstalled."


