#!/usr/bin/env bash
# setup-linux.sh — one-time setup for ais-runner on Linux (Debian/Ubuntu/Fedora/Arch)
#
# Installs: WebKitGTK (runtime), Node.js 20, Azure CLI, Azurite, Azure Functions Core Tools
# Already-installed tools are skipped automatically.
#
# Usage (from the extracted release archive):
#   chmod +x setup-linux.sh && sudo ./setup-linux.sh

set -e

info()  { echo -e "\033[34m[info]\033[0m  $*"; }
ok()    { echo -e "\033[32m[ok]\033[0m    $*"; }
skip()  { echo -e "\033[33m[skip]\033[0m  $*"; }
warn()  { echo -e "\033[33m[warn]\033[0m  $*"; }
err()   { echo -e "\033[31m[error]\033[0m $*"; exit 1; }

# ── Detect distro ─────────────────────────────────────────────────────────────
if   command -v apt-get &>/dev/null; then DISTRO=debian
elif command -v dnf     &>/dev/null; then DISTRO=fedora
elif command -v pacman  &>/dev/null; then DISTRO=arch
else err "Unsupported distro — install dependencies manually (see README)"
fi

info "Detected distro family: $DISTRO"

# ── Runtime dependencies (WebKitGTK + libxdo) ────────────────────────────────
case "$DISTRO" in
  debian)
    PKGS=()
    dpkg -l libwebkit2gtk-4.1-0 &>/dev/null || dpkg -l libwebkit2gtk-4.0-0 &>/dev/null || PKGS+=(libwebkit2gtk-4.1-0)
    dpkg -l libxdo3 &>/dev/null || PKGS+=(libxdo3)
    if [ ${#PKGS[@]} -gt 0 ]; then
      info "Installing runtime libs: ${PKGS[*]}"
      apt-get update -qq
      apt-get install -y "${PKGS[@]}" 2>/dev/null || apt-get install -y libwebkit2gtk-4.0-0 libxdo3
      ok "Runtime libs installed"
    else
      skip "Runtime libs already installed"
    fi
    ;;
  fedora)
    if ! rpm -q webkit2gtk4.1 &>/dev/null && ! rpm -q webkit2gtk3 &>/dev/null; then
      info "Installing WebKitGTK runtime..."
      dnf install -y webkit2gtk4.1 2>/dev/null || dnf install -y webkit2gtk3
      ok "WebKitGTK installed"
    else
      skip "WebKitGTK already installed"
    fi
    # xdotool provides libxdo on Fedora
    rpm -q xdotool &>/dev/null || dnf install -y xdotool
    ;;
  arch)
    if ! pacman -Qi webkit2gtk-4.1 &>/dev/null && ! pacman -Qi webkit2gtk &>/dev/null; then
      info "Installing WebKitGTK runtime..."
      pacman -S --noconfirm webkit2gtk-4.1 2>/dev/null || pacman -S --noconfirm webkit2gtk
      ok "WebKitGTK installed"
    else
      skip "WebKitGTK already installed"
    fi
    pacman -Qi xdotool &>/dev/null || pacman -S --noconfirm xdotool
    ;;
esac

# ── Node.js 20 ────────────────────────────────────────────────────────────────
install_node() {
  case "$DISTRO" in
    debian)
      info "Installing Node.js 20 via NodeSource..."
      curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
      apt-get install -y nodejs
      ;;
    fedora)
      info "Installing Node.js 20 via NodeSource..."
      curl -fsSL https://rpm.nodesource.com/setup_20.x | bash -
      dnf install -y nodejs
      ;;
    arch)
      info "Installing Node.js..."
      pacman -S --noconfirm nodejs npm
      ;;
  esac
}

if ! command -v node &>/dev/null; then
  install_node
  ok "Node.js installed ($(node --version))"
else
  NODE_MAJOR=$(node --version | sed 's/v\([0-9]*\).*/\1/')
  if [ "$NODE_MAJOR" -lt 18 ]; then
    warn "Node.js $(node --version) is older than v18 — upgrading to v20..."
    install_node
    ok "Node.js upgraded ($(node --version))"
  else
    skip "Node.js already installed ($(node --version))"
  fi
fi

# ── Azure CLI ─────────────────────────────────────────────────────────────────
if ! command -v az &>/dev/null; then
  info "Installing Azure CLI..."
  case "$DISTRO" in
    debian)
      curl -sL https://aka.ms/InstallAzureCLIDeb | bash
      ;;
    fedora)
      rpm --import https://packages.microsoft.com/keys/microsoft.asc
      dnf install -y https://packages.microsoft.com/config/rhel/9.0/packages-microsoft-prod.rpm
      dnf install -y azure-cli
      ;;
    arch)
      # AUR — requires yay or paru
      if command -v yay &>/dev/null; then
        yay -S --noconfirm azure-cli
      elif command -v paru &>/dev/null; then
        paru -S --noconfirm azure-cli
      else
        warn "Azure CLI not installed — install manually via AUR (yay -S azure-cli) or pip"
      fi
      ;;
  esac
  command -v az &>/dev/null && ok "Azure CLI installed ($(az version --query '"azure-cli"' -o tsv))"
else
  skip "Azure CLI already installed ($(az version --query '"azure-cli"' -o tsv 2>/dev/null || az --version | head -1))"
fi

# ── Azurite ───────────────────────────────────────────────────────────────────
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

# ── Desktop shortcut (optional) ───────────────────────────────────────────────
BINARY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DESKTOP_FILE="/usr/share/applications/ais-runner.desktop"
if [[ -f "$BINARY_DIR/ais-runner" && ! -f "$DESKTOP_FILE" ]]; then
  info "Creating .desktop launcher..."
  cat > "$DESKTOP_FILE" <<EOF
[Desktop Entry]
Name=AIS Runner
Comment=Azure Integration Services local development runner
Exec=$BINARY_DIR/ais-runner
Icon=utilities-terminal
Terminal=false
Type=Application
Categories=Development;
EOF
  ok "Desktop shortcut created"
fi

echo ""
echo "Setup complete. Run ./ais-runner to start the app."
