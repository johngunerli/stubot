#!/usr/bin/env bash
# Thin wrapper — prefers the installed `stubot` keyword, else cargo run.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

if [[ ! -f .env && -f .env.example ]]; then
  cp .env.example .env
fi

if command -v stubot >/dev/null 2>&1; then
  exec stubot "$@"
fi

exec cargo run --release -- "$@"
