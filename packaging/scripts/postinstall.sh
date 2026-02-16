#!/usr/bin/env bash
set -e

# Check for existing tarball/manual install
for f in /usr/local/bin/beam-server /usr/local/bin/beam-agent; do
    if [ -f "$f" ] && ! dpkg -S "$f" >/dev/null 2>&1; then
        echo "WARNING: $f exists but is not owned by any package."
        echo "You may have a previous manual/tarball install."
        echo "Consider running: sudo /path/to/beam/scripts/uninstall.sh"
        echo ""
    fi
done

# Set real-time scheduling capability on beam-agent for consistent frame pacing
if command -v setcap >/dev/null 2>&1; then
    setcap cap_sys_nice=ep /usr/local/bin/beam-agent 2>/dev/null || true
fi

# Reload udev rules for uinput access
udevadm control --reload-rules 2>/dev/null || true
udevadm trigger 2>/dev/null || true

# Reload systemd
systemctl daemon-reload 2>/dev/null || true

# Start/restart service
if [ -d /run/systemd/system ]; then
    if systemctl is-active --quiet beam 2>/dev/null; then
        # Upgrade: restart the running service
        systemctl restart beam 2>/dev/null || true
    else
        # Fresh install: enable and start
        systemctl enable --now beam 2>/dev/null || true
    fi
fi

# Determine port from config
PORT=$(grep -oP '^\s*port\s*=\s*\K\d+' /etc/beam/beam.toml 2>/dev/null || echo "8444")
HOSTNAME=$(hostname 2>/dev/null || echo "localhost")

echo ""
echo "============================================"
echo "  Beam Remote Desktop is running!"
echo "============================================"
echo ""
echo "  Open:    https://${HOSTNAME}:${PORT}"
echo "  Login:   Any Linux user account"
echo "  Status:  sudo systemctl status beam"
echo "  Logs:    journalctl -u beam -f"
echo "  Config:  /etc/beam/beam.toml"
echo "  Check:   beam-doctor"
echo ""
echo "  NOTE: Uses self-signed TLS by default."
echo "        Configure real certs in /etc/beam/beam.toml"
echo ""
