#!/usr/bin/env bash
# Beam Remote Desktop — one-liner install script
# Usage: curl -fsSL https://frecar.github.io/beam/install | sudo bash
set -euo pipefail

REPO_URL="https://frecar.github.io/beam"

echo "Installing Beam Remote Desktop..."

# Add GPG key
mkdir -p /etc/apt/keyrings
curl -fsSL "${REPO_URL}/gpg/beam.gpg" | gpg --dearmor -o /etc/apt/keyrings/beam.gpg

# Add APT source
ARCH=$(dpkg --print-architecture)
cat > /etc/apt/sources.list.d/beam.list << EOF
deb [arch=${ARCH} signed-by=/etc/apt/keyrings/beam.gpg] ${REPO_URL} ./
EOF

# Install
apt-get update -qq
apt-get install -y beam

echo ""
echo "Beam installed! Open https://$(hostname):8444 in your browser."
