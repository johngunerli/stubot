#!/usr/bin/env bash
# Install the `stubot` keyword onto PATH (~/.cargo/bin/stubot).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found. Install Rust from https://rustup.rs" >&2
  exit 1
fi

if [[ ! -f .env && -f .env.example ]]; then
  cp .env.example .env
  echo "created .env from .env.example — set LLM_API_KEY"
fi

# Drop old keyword if present
rm -f "${CARGO_HOME:-$HOME/.cargo}/bin/bot"

echo "→ cargo install --path . --force (binary name: stubot)"
cargo install --path . --force --locked 2>/dev/null || cargo install --path . --force

BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
echo
echo "Installed: $BIN_DIR/stubot"
if ! command -v stubot >/dev/null 2>&1; then
  echo
  echo "Add cargo bin to your PATH (zsh):"
  echo "  echo 'export PATH=\"\$HOME/.cargo/bin:\$PATH\"' >> ~/.zshrc && source ~/.zshrc"
fi

echo
echo "Try:"
echo "  stubot"
echo "  stubot --prompt 'hello'"
echo "  stubot --help"
