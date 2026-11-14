#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
docker build -t stubot-base:latest -f "$ROOT/docker/Dockerfile.base" "$ROOT/docker"
docker build -t stubot-sysadmin:latest -f "$ROOT/docker/Dockerfile.sysadmin" "$ROOT/docker"
echo "Built stubot-base:latest and stubot-sysadmin:latest"
