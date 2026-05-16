#!/usr/bin/env bash
# Tear down the tailscale serve config registered by infra/serve.sh.
set -euo pipefail

tailscale serve reset
tailscale serve status
