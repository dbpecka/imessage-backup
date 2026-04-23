#!/usr/bin/env bash
# Xcode Cloud post-clone hook — installs toolchain, fetches sibling dep, builds Tauri.
set -euo pipefail

REPO_DIR="/Volumes/workspace/repository"
WORKSPACE_DIR="/Volumes/workspace"

# --- Rust ---
# The rustup curl installer can't resolve DNS in Xcode Cloud; use Homebrew instead.
if ! command -v rustc &>/dev/null; then
    brew install rust
fi

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
# Xcode Cloud sets CI=TRUE (uppercase); Tauri's CLI only accepts lowercase true/false.
CI=true npm run tauri build
