#!/usr/bin/env bash
set -e

# Stop and disable beam service before removing files
if [ -d /run/systemd/system ]; then
    systemctl stop beam 2>/dev/null || true
    systemctl disable beam 2>/dev/null || true
fi
