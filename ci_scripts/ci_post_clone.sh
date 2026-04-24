#!/usr/bin/env bash
# Xcode Cloud post-clone hook — installs toolchain, fetches sibling dep, builds Tauri.
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

# --- Sibling repo: imessage-exporter fork ---
# Cargo.toml has a path dep: path = "../../imessage-exporter/imessage-database"
# Relative to src-tauri/ that resolves to /Volumes/workspace/imessage-exporter/.
EXPORTER_DIR="$WORKSPACE_DIR/imessage-exporter"
if [ ! -d "$EXPORTER_DIR" ]; then
    # Derive the GitHub user from this repo's origin URL (works for both HTTPS and SSH remotes).
    ORIGIN=$(git -C "$REPO_DIR" remote get-url origin)
    GITHUB_USER=$(echo "$ORIGIN" | sed -E 's|.*github\.com[:/]([^/]+)/.*|\1|')
    git clone --depth 1 --branch develop \
        "https://github.com/$GITHUB_USER/imessage-exporter.git" \
        "$EXPORTER_DIR"
fi

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
