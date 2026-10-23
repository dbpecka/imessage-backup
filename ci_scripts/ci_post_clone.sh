#!/usr/bin/env bash
# Xcode Cloud post-clone hook — installs toolchain, builds Tauri.
set -euo pipefail

REPO_DIR="/Volumes/workspace/repository"
WORKSPACE_DIR="/Volumes/workspace"

# --- Rust ---
# Use Homebrew's rustup formula to avoid the curl installer (DNS is blocked in
# Xcode Cloud). We need rustup (not the bare rust formula) to add cross-compile
# targets required for a universal binary.
if ! command -v rustup &>/dev/null; then
    brew install rustup
    rustup-init -y --no-modify-path --default-toolchain stable
fi
source "$HOME/.cargo/env"
rustup target add aarch64-apple-darwin x86_64-apple-darwin

# --- Node.js ---
if ! command -v node &>/dev/null; then
    brew install node
fi

# --- imessage-exporter fork ---
# A git dependency pinned to an exact commit in src-tauri/Cargo.toml; cargo
# fetches it from GitHub during the build, so no separate clone is needed.

# --- Build ---
cd "$REPO_DIR"
npm ci

# Xcode Cloud's network only allows Apple/GitHub hosts by default; index.crates.io
# (Fastly CDN) is blocked. Switch Cargo to the git-based crates.io index on GitHub,
# which is accessible. CARGO_NET_GIT_FETCH_WITH_CLI avoids libgit2 auth quirks.
export CARGO_REGISTRIES_CRATES_IO_PROTOCOL=git
export CARGO_NET_GIT_FETCH_WITH_CLI=true

# Xcode Cloud sets CI=TRUE (uppercase); Tauri's CLI only accepts lowercase true/false.
CI=true npm run tauri build -- --target universal-apple-darwin
