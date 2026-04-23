#!/usr/bin/env bash
# Xcode Cloud post-clone hook — installs Rust + Node, then builds Tauri.
set -euo pipefail

# Rust
if ! command -v rustup &>/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --default-toolchain stable
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"

# Node (use system install; fall back to nvm)
if ! command -v node &>/dev/null; then
    curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.39.0/install.sh | bash
    # shellcheck disable=SC1091
    source "$HOME/.nvm/nvm.sh"
    nvm install --lts
fi

npm ci
npm run tauri build
