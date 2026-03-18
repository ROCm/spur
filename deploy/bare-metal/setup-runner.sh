#!/bin/bash
# Install a GitHub Actions self-hosted runner on mi300.
#
# Run this once on mi300 (ssh mi300, then bash setup-runner.sh <REPO> <TOKEN>).
#
# Get a registration token:
#   GitHub → repo → Settings → Actions → Runners → "New self-hosted runner"
#   Copy the token from the ./config.sh command shown there (--token <TOKEN>).
#   Tokens expire after 1 hour.
#
# Usage:
#   bash setup-runner.sh <owner/repo> <registration-token>
#
# Example:
#   bash setup-runner.sh powderluv/spur AABBBCCCC...
#
# After setup, the runner appears under Settings → Actions → Runners as "mi300-runner"
# with label "mi300x".  It connects outbound to GitHub (port 443) — no inbound ports
# are needed and no IPs appear in the workflow files.

set -euo pipefail

REPO="${1:?Usage: $0 <owner/repo> <registration-token>}"
TOKEN="${2:?Usage: $0 <owner/repo> <registration-token>}"

RUNNER_VERSION="2.321.0"
RUNNER_DIR="$HOME/actions-runner"
RUNNER_ARCH="linux-x64"
RUNNER_TARBALL="actions-runner-${RUNNER_ARCH}-${RUNNER_VERSION}.tar.gz"
RUNNER_URL="https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/${RUNNER_TARBALL}"

echo "==> Creating runner directory: $RUNNER_DIR"
mkdir -p "$RUNNER_DIR"
cd "$RUNNER_DIR"

echo "==> Downloading GitHub Actions runner v${RUNNER_VERSION}"
curl -sSfLO "$RUNNER_URL"
tar xzf "$RUNNER_TARBALL"
rm "$RUNNER_TARBALL"

echo "==> Configuring runner"
./config.sh \
  --url "https://github.com/${REPO}" \
  --token "$TOKEN" \
  --name "mi300-runner" \
  --labels "self-hosted,linux,x64,mi300x" \
  --work "_work" \
  --unattended \
  --replace

echo "==> Installing runner as systemd service"
# svc.sh needs sudo; the service will run as the current user.
sudo ./svc.sh install "$USER"
sudo ./svc.sh start

echo ""
echo "Runner installed and started."
echo "Verify at: https://github.com/${REPO}/settings/actions/runners"
echo ""
echo "Useful commands:"
echo "  sudo $RUNNER_DIR/svc.sh status   # check service status"
echo "  sudo $RUNNER_DIR/svc.sh stop     # stop runner"
echo "  sudo $RUNNER_DIR/svc.sh start    # start runner"
echo "  journalctl -u actions.runner.*   # view logs"
