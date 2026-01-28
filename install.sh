#!/bin/bash
#
# COKACDIR Installer
# Usage: curl -fsSL https://cokacdir.cokac.com/install.sh | bash
#

set -e

BINARY_NAME="cokacdir"
BASE_URL="https://cokacdir.cokac.com/dist"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info() {
    echo -e "${BLUE}→${NC} $1"
}

success() {
    echo -e "${GREEN}✓${NC} $1"
}

warn() {
    echo -e "${YELLOW}!${NC} $1"
}

error() {
    echo -e "${RED}✗${NC} $1"
    exit 1
}

# Detect OS
detect_os() {
    local os
    os="$(uname -s)"
    case "$os" in
        Linux*)  echo "linux" ;;
        Darwin*) echo "macos" ;;
        *)       error "Unsupported OS: $os" ;;
    esac
}

# Detect architecture
detect_arch() {
    local arch
    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64)  echo "x86_64" ;;
        aarch64|arm64) echo "aarch64" ;;
        *)             error "Unsupported architecture: $arch" ;;
    esac
}

# Get install directory
get_install_dir() {
    # Prefer /usr/local/bin (always in PATH)
    if [ -d "/usr/local/bin" ]; then
        echo "/usr/local/bin"
    else
        # Fallback to ~/.local/bin
        mkdir -p "$HOME/.local/bin"
        echo "$HOME/.local/bin"
    fi
}

# Check if command exists
has_cmd() {
    command -v "$1" >/dev/null 2>&1
}

# Download file
download() {
    local url="$1"
    local dest="$2"

    if has_cmd curl; then
        curl -fsSL "$url" -o "$dest"
    elif has_cmd wget; then
        wget -q "$url" -O "$dest"
    else
        error "curl or wget is required"
    fi
}

main() {
    echo ""
    echo "=================================="
    echo "  COKACDIR Installer"
    echo "=================================="
    echo ""

    # Detect platform
    local os arch
    os="$(detect_os)"
    arch="$(detect_arch)"

    info "Detected: $os-$arch"

    # Build download URL
    local filename="${BINARY_NAME}-${os}-${arch}"
    local url="${BASE_URL}/${filename}"

    info "Downloading from: $url"

    # Create temp file
    local tmpfile
    tmpfile="$(mktemp)"
    trap 'rm -f "$tmpfile"' EXIT

    # Download
    if ! download "$url" "$tmpfile"; then
        error "Failed to download $url"
    fi

    # Make executable
    chmod +x "$tmpfile"

    # Get install directory
    local install_dir
    install_dir="$(get_install_dir)"
    local install_path="${install_dir}/${BINARY_NAME}"

    info "Installing to: $install_path"

    # Install
    if [ -w "$install_dir" ]; then
        mv "$tmpfile" "$install_path"
    else
        info "Requesting sudo access..."
        sudo mv "$tmpfile" "$install_path"
    fi

    # Verify installation
    if [ -x "$install_path" ]; then
        success "Installed successfully!"
        echo ""

        # Check if in PATH
        if ! echo "$PATH" | grep -q "$install_dir"; then
            warn "$install_dir is not in your PATH"
            echo ""
            echo "Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
            echo "  export PATH=\"$install_dir:\$PATH\""
            echo ""
        fi

        # Show version
        if has_cmd "$BINARY_NAME"; then
            info "Version: $("$BINARY_NAME" --version 2>/dev/null || echo "unknown")"
        fi

        echo ""
        success "Run '$BINARY_NAME --help' to get started"
    else
        error "Installation failed"
    fi
}

main "$@"
