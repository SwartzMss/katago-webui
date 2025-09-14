#!/usr/bin/env bash
set -euo pipefail

# KataGo WebUI - systemd installer (system-wide)
# Usage: sudo ./scripts/install-service.sh

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd -P)"
BACKEND_DIR="$REPO_DIR/backend"
UNIT_NAME="katago-webui.service"
UNIT_PATH="/etc/systemd/system/${UNIT_NAME}"
RUN_USER="${SUDO_USER:-$(whoami)}"

echo "[1/4] Building backend (release) as $RUN_USER ..."
# Ensure cargo exists for RUN_USER; try sourcing ~/.cargo/env when present
if ! sudo -u "$RUN_USER" bash -lc 'command -v cargo >/dev/null 2>&1 || { [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"; command -v cargo >/dev/null 2>&1; }'; then
  cat >&2 <<'EOM'
Rust toolchain not found for the target user. Please install it first:
  curl -sSf https://sh.rustup.rs | sh -s -- -y
  source "$HOME/.cargo/env"
Then re-run: sudo ./scripts/install-service.sh
EOM
  exit 1
fi

# Build backend as RUN_USER so it uses that user's cargo toolchain/cache
sudo -u "$RUN_USER" bash -lc "cd '$BACKEND_DIR'; [ -f \"$HOME/.cargo/env\" ] && source \"$HOME/.cargo/env\"; cargo build --release"

BIN_PATH="$BACKEND_DIR/target/release/backend"
if [[ ! -x "$BIN_PATH" ]]; then
  echo "Build failed: $BIN_PATH not found" >&2
  exit 1
fi

echo "[2/4] Writing systemd unit to $UNIT_PATH ..."
cat > "$UNIT_PATH" <<EOF
[Unit]
Description=KataGo WebUI Backend
After=network.target

[Service]
Type=simple
User=$RUN_USER
WorkingDirectory=$REPO_DIR
Environment=RUST_LOG=info
# Optional env files
EnvironmentFile=-$BACKEND_DIR/.env
EnvironmentFile=-$REPO_DIR/.env
ExecStart=$BIN_PATH
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF

echo "[3/4] Reloading systemd and enabling service ..."
systemctl daemon-reload
systemctl enable --now "$UNIT_NAME"

echo "[4/4] Service status (short):"
systemctl --no-pager --full status "$UNIT_NAME" | sed -n '1,40p'

echo "\nInstalled. Logs: journalctl -u $UNIT_NAME -f"


