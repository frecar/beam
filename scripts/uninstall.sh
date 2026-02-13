#!/usr/bin/env bash
set -euo pipefail

# Beam Remote Desktop - Uninstaller

log() { echo -e "\033[1;34m[beam]\033[0m $*"; }
ok()  { echo -e "\033[1;32m[beam]\033[0m $*"; }

if [[ $EUID -ne 0 ]]; then
    echo "This script must be run as root (sudo)"
    exit 1
fi

log "Stopping Beam service..."
systemctl stop beam 2>/dev/null || true
systemctl disable beam 2>/dev/null || true

log "Removing binaries..."
rm -f /usr/local/bin/beam-server
rm -f /usr/local/bin/beam-agent

log "Removing systemd service..."
rm -f /etc/systemd/system/beam.service
systemctl daemon-reload

log "Removing web assets..."
rm -rf /usr/share/beam

log "Removing udev rules..."
rm -f /etc/udev/rules.d/99-beam-uinput.rules
udevadm control --reload-rules 2>/dev/null || true

# Optionally remove config
if [ -t 0 ]; then
    read -p "Remove configuration (/etc/beam)? [y/N] " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        rm -rf /etc/beam
        log "Configuration removed"
    else
        log "Configuration kept at /etc/beam"
    fi
else
    log "Non-interactive mode: keeping /etc/beam. Remove manually if desired."
fi

ok "Beam uninstalled successfully."
