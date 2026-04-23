#!/usr/bin/env bash
# Production build script for Bubble Wrap.
#
# Modes
#   --local        Build locally with the Tauri CLI
#   --xcode-cloud  Trigger a build on Xcode Cloud via App Store Connect API
#   --both         Local build, then trigger Xcode Cloud
#   --setup-ci     Generate ci_scripts/ for Xcode Cloud (run once)
#
# Local signing env vars (all optional if the keychain already has the cert):
#   APPLE_SIGNING_IDENTITY   Default: "Developer ID Application: Derek Pecka (VNU7JJY79M)"
#   APPLE_ID                 Apple ID for notarization (e.g. derek@pecka.io)
#   APPLE_PASSWORD           App-specific password for notarization
#   APPLE_TEAM_ID            Default: VNU7JJY79M
#
# Xcode Cloud env vars (required for --xcode-cloud):
#   ASC_KEY_ID               App Store Connect API key ID
#   ASC_ISSUER_ID            App Store Connect issuer ID
#   ASC_PRIVATE_KEY_PATH     Path to the .p8 private key file
#   XCODE_CLOUD_WORKFLOW_ID  Xcode Cloud workflow ID to trigger

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

BOLD='\033[1m'; BLUE='\033[0;34m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; NC='\033[0m'
info()    { echo -e "${BLUE}==>${NC} ${BOLD}$*${NC}"; }
success() { echo -e "${GREEN}  ✓${NC} $*"; }
warn()    { echo -e "${YELLOW}  !${NC} $*"; }
die()     { echo -e "${RED}Error:${NC} $*" >&2; exit 1; }

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Options:
  --local          Build locally with Tauri CLI
  --xcode-cloud    Trigger a build on Xcode Cloud
  --both           Local build, then trigger Xcode Cloud
  --setup-ci       Generate ci_scripts/ for Xcode Cloud (run once)
  -h, --help       Show this help
EOF
}

DO_LOCAL=false
DO_XCODE_CLOUD=false
DO_SETUP_CI=false

[[ $# -eq 0 ]] && usage && exit 1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --local)       DO_LOCAL=true ;;
        --xcode-cloud) DO_XCODE_CLOUD=true ;;
        --both)        DO_LOCAL=true; DO_XCODE_CLOUD=true ;;
        --setup-ci)    DO_SETUP_CI=true ;;
        -h|--help)     usage; exit 0 ;;
        *)             die "Unknown option: $1"; usage; exit 1 ;;
    esac
    shift
done

# ---------------------------------------------------------------------------
# Local build
# ---------------------------------------------------------------------------
run_local_build() {
    info "Installing npm dependencies"
    npm ci --prefix "$SCRIPT_DIR"

    info "Building Tauri application (release)"
    cd "$SCRIPT_DIR"

    # Collect any signing env vars the caller has set; pass them through so
    # that Tauri's bundler can sign and notarize without touching the shell env
    # of unrelated processes.
    local -a extra_env=()
    local signing_identity="${APPLE_SIGNING_IDENTITY:-Developer ID Application: Derek Pecka (VNU7JJY79M)}"
    extra_env+=("APPLE_SIGNING_IDENTITY=$signing_identity")
    [[ -n "${APPLE_ID:-}"       ]] && extra_env+=("APPLE_ID=$APPLE_ID")
    [[ -n "${APPLE_PASSWORD:-}" ]] && extra_env+=("APPLE_PASSWORD=$APPLE_PASSWORD")
    local team_id="${APPLE_TEAM_ID:-VNU7JJY79M}"
    extra_env+=("APPLE_TEAM_ID=$team_id")

    env "${extra_env[@]}" npm run tauri build

    info "Artifacts"
    local bundle_dir="$SCRIPT_DIR/src-tauri/target/release/bundle"
    while IFS= read -r -d '' artifact; do
        success "$artifact"
    done < <(find "$bundle_dir" \( -name "*.app" -o -name "*.dmg" \) -print0 2>/dev/null)
}

# ---------------------------------------------------------------------------
# Xcode Cloud — trigger via App Store Connect API
# ---------------------------------------------------------------------------
run_xcode_cloud_build() {
    local -a required_vars=(ASC_KEY_ID ASC_ISSUER_ID ASC_PRIVATE_KEY_PATH XCODE_CLOUD_WORKFLOW_ID)
    local -a missing=()
    for v in "${required_vars[@]}"; do
        [[ -z "${!v:-}" ]] && missing+=("$v")
    done
    [[ ${#missing[@]} -gt 0 ]] && die "Missing required env vars: ${missing[*]}"
    [[ -f "${ASC_PRIVATE_KEY_PATH}" ]] || die "Private key not found: $ASC_PRIVATE_KEY_PATH"

    info "Generating App Store Connect API JWT"

    # Generate an ES256 JWT using openssl for signing (no Python packages needed).
    # The DER-encoded ECDSA signature is decoded into the raw r||s form that JWT expects.
    local jwt
    jwt=$(python3 - <<'PYTHON'
import base64, json, os, subprocess, time

key_id    = os.environ["ASC_KEY_ID"]
issuer_id = os.environ["ASC_ISSUER_ID"]
key_path  = os.environ["ASC_PRIVATE_KEY_PATH"]

def b64url(data):
    if isinstance(data, str):
        data = data.encode()
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()

now = int(time.time())
header  = b64url(json.dumps({"alg": "ES256", "kid": key_id, "typ": "JWT"}, separators=(",", ":")))
payload = b64url(json.dumps({"iss": issuer_id, "iat": now, "exp": now + 1200, "aud": "appstoreconnect-v1"}, separators=(",", ":")))
signing_input = f"{header}.{payload}"

# openssl produces a DER-encoded ECDSA signature; JWT wants raw r||s (32 bytes each).
der = subprocess.check_output(
    ["openssl", "dgst", "-sha256", "-sign", key_path],
    input=signing_input.encode(),
)

# Parse DER: 30 <len> 02 <rlen> <r> 02 <slen> <s>
i = 2
assert der[i] == 0x02, "Expected DER integer tag for r"
i += 1
r_len = der[i]; i += 1
r = der[i : i + r_len]; i += r_len
assert der[i] == 0x02, "Expected DER integer tag for s"
i += 1
s_len = der[i]; i += 1
s = der[i : i + s_len]

# Strip the leading sign byte DER may add, then left-pad to 32 bytes.
r = r.lstrip(b"\x00").rjust(32, b"\x00")
s = s.lstrip(b"\x00").rjust(32, b"\x00")

print(f"{signing_input}.{b64url(r + s)}")
PYTHON
)

    info "Triggering Xcode Cloud workflow: $XCODE_CLOUD_WORKFLOW_ID"

    local response
    response=$(curl --silent --fail-with-body \
        --request POST \
        --header "Authorization: Bearer $jwt" \
        --header "Content-Type: application/json" \
        --data '{
            "data": {
                "type": "ciBuildRuns",
                "relationships": {
                    "workflow": {
                        "data": { "type": "ciWorkflows", "id": "'"$XCODE_CLOUD_WORKFLOW_ID"'" }
                    }
                }
            }
        }' \
        "https://api.appstoreconnect.apple.com/v1/ciBuildRuns")

    local build_id
    build_id=$(python3 -c "import sys, json; print(json.load(sys.stdin)['data']['id'])" <<< "$response")

    local team_id="${APPLE_TEAM_ID:-VNU7JJY79M}"
    success "Build queued — ID: $build_id"
    success "Track at: https://appstoreconnect.apple.com/teams/$team_id/xcode-cloud"
}

# ---------------------------------------------------------------------------
# One-time Xcode Cloud CI scaffolding
# ---------------------------------------------------------------------------
run_setup_ci() {
    local ci_dir="$SCRIPT_DIR/ci_scripts"
    mkdir -p "$ci_dir"

    cat > "$ci_dir/ci_post_clone.sh" <<'SCRIPT'
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
SCRIPT
    chmod +x "$ci_dir/ci_post_clone.sh"

    success "Created: $ci_dir/ci_post_clone.sh"
    echo
    warn "Xcode Cloud requires an Xcode project (.xcodeproj or .xcworkspace) at the repo root."
    warn "Tauri does not generate one for macOS-only targets. Options:"
    warn "  1. Create a minimal Xcode app target as a CI wrapper (the Tauri binary is the real product)."
    warn "  2. Run 'npm run tauri ios init' if you add iOS support — that generates gen/apple/."
    warn "  3. Use GitHub Actions or Bitrise as an alternative for macOS-only builds."
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
$DO_SETUP_CI     && run_setup_ci
$DO_LOCAL        && run_local_build
$DO_XCODE_CLOUD  && run_xcode_cloud_build

echo
success "Done."
