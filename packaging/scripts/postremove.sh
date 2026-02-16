#!/usr/bin/env bash
set -e

# Reload systemd after service file removal
systemctl daemon-reload 2>/dev/null || true

# On purge: remove configuration and runtime data
if [ "$1" = "purge" ]; then
    rm -rf /var/lib/beam
    rm -rf /etc/beam
fi
