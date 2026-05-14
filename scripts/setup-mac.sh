#!/bin/bash
set -e

# One-time setup for ais-runner on macOS.
# Installs: Homebrew, Node.js 20, Azure CLI, Azurite 3.30.0, Azure Functions Core Tools 4.x
# Already-installed tools are skipped automatically.

info()  { echo "[info]  $*"; }
ok()    { echo "[ok]    $*"; }
skip()  { echo "[skip]  $*"; }
warn()  { echo "[warn]  $*"; }

# ── Homebrew ─────────────────────────────────────────────────────────────────
if ! command -v brew &>/dev/null; then
    info "Installing Homebrew..."
    /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    # Add brew to PATH for the rest of this script (Apple Silicon path)
    eval "$(/opt/homebrew/bin/brew shellenv 2>/dev/null || /usr/local/bin/brew shellenv)"
    ok "Homebrew installed"
else
    skip "Homebrew already installed ($(brew --version | head -1))"
fi

# ── Node.js 20 ───────────────────────────────────────────────────────────────
if ! command -v node &>/dev/null; then
    info "Installing Node.js 20..."
    brew install node@20
    brew link --force --overwrite node@20
    ok "Node.js installed ($(node --version))"
else
    NODE_MAJOR=$(node --version | sed 's/v\([0-9]*\).*/\1/')
    if [ "$NODE_MAJOR" -lt 18 ]; then
        warn "Node.js $(node --version) is older than v18 — upgrading to v20..."
        brew install node@20
        brew link --force --overwrite node@20
        ok "Node.js upgraded ($(node --version))"
    else
        skip "Node.js already installed ($(node --version))"
    fi
fi

# ── Azure CLI ────────────────────────────────────────────────────────────────
if ! command -v az &>/dev/null; then
    info "Installing Azure CLI..."
    brew install azure-cli
    ok "Azure CLI installed ($(az version --query '"azure-cli"' -o tsv))"
else
    skip "Azure CLI already installed ($(az version --query '"azure-cli"' -o tsv 2>/dev/null || az --version | head -1))"
fi

# ── Azurite ──────────────────────────────────────────────────────────────────
AZURITE_TARGET="3.30.0"
if ! command -v azurite &>/dev/null; then
    info "Installing Azurite $AZURITE_TARGET..."
    npm install -g "azurite@$AZURITE_TARGET"
    ok "Azurite installed"
else
    INSTALLED=$(azurite --version 2>/dev/null || echo "unknown")
    skip "Azurite already installed ($INSTALLED)"
fi

# ── Azure Functions Core Tools 4.x ───────────────────────────────────────────
if ! command -v func &>/dev/null; then
    info "Installing Azure Functions Core Tools 4.x..."
    npm install -g azure-functions-core-tools@4 --unsafe-perm true
    ok "func installed ($(func --version))"
else
    skip "func already installed ($(func --version))"
fi

echo ""
echo "Setup complete. Run ./ais-runner to start the app."
