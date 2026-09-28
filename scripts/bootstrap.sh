#!/usr/bin/env bash
set -euo pipefail
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile default
fi
rustup toolchain install stable
rustup target add aarch64-apple-darwin x86_64-apple-darwin
rustup component add clippy rustfmt
brew install xcodegen cmake meson ninja pkg-config opus
echo "bootstrap ok: $(rustc --version), $(xcodegen --version 2>/dev/null || true)"
