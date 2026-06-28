#!/bin/bash
#
# COKACCTL Installer
# Usage: curl -fsSL https://raw.githubusercontent.com/kstost/cokacctl/refs/heads/main/manage.sh | bash
#

set -e

BINARY_NAME="cokacctl"
BASE_URL="https://raw.githubusercontent.com/kstost/cokacctl/refs/heads/main/dist_beta"

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

# Shell wrapper function to add
SHELL_FUNC='cokacctl() { command cokacctl "$@" && cd "$(cat ~/.cokacctl/lastdir 2>/dev/null || pwd)"; }'

# Get shell config file
fallback_shell_config() {
    if [ -f "$HOME/.zshrc" ]; then
        echo "$HOME/.zshrc"
    elif [ -f "$HOME/.bashrc" ]; then
        echo "$HOME/.bashrc"
    elif [ -f "$HOME/.bash_profile" ]; then
        echo "$HOME/.bash_profile"
    elif [ -z "${SHELL:-}" ]; then
        case "$(uname -s)" in
            Darwin*) echo "$HOME/.zshrc" ;;
            *)       echo "$HOME/.bashrc" ;;
        esac
    else
        echo ""
    fi
}

get_shell_config() {
    local shell_name
    shell_name=""
    if [ -n "${SHELL:-}" ]; then
        shell_name="$(basename "$SHELL")"
    fi

    case "$shell_name" in
        bash)
            if [ -f "$HOME/.bashrc" ]; then
                echo "$HOME/.bashrc"
            elif [ -f "$HOME/.bash_profile" ]; then
                echo "$HOME/.bash_profile"
            else
                echo "$HOME/.bashrc"
            fi
            ;;
        zsh)
            echo "$HOME/.zshrc"
            ;;
        *)
            fallback_shell_config
            ;;
    esac
}

# Setup shell wrapper function
setup_shell() {
    local config_file
    config_file="$(get_shell_config)"

    if [ -z "$config_file" ]; then
        return
    fi

    # Check if already configured
    if [ -f "$config_file" ] && grep -q "cokacctl()" "$config_file"; then
        return
    fi

    # Create file if not exists
    if [ ! -f "$config_file" ]; then
        touch "$config_file"
    fi

    # Add function
    echo "" >> "$config_file"
    echo "# cokacctl - cd to last directory on exit" >> "$config_file"
    echo "$SHELL_FUNC" >> "$config_file"
}

main() {
    # Detect platform
    local os arch
    os="$(detect_os)"
    arch="$(detect_arch)"

    info "Downloading cokacctl ($os-$arch)..."

    # Build download URL
    local filename="${BINARY_NAME}-${os}-${arch}"
    local url="${BASE_URL}/${filename}"

    # Create temp file
    local tmpfile
    tmpfile="$(mktemp)"
    trap 'rm -f "$tmpfile"' EXIT

    # Download
    if ! download "$url" "$tmpfile"; then
        error "Download failed"
    fi

    # Make executable
    chmod +x "$tmpfile"

    # Get install directory
    local install_dir
    install_dir="$(get_install_dir)"
    local install_path="${install_dir}/${BINARY_NAME}"

    # Install
    if [ -w "$install_dir" ]; then
        mv "$tmpfile" "$install_path"
    else
        sudo mv "$tmpfile" "$install_path"
    fi

    # Verify installation
    if [ -x "$install_path" ]; then
        # Check if in PATH
        if ! echo "$PATH" | grep -q "$install_dir"; then
            warn "Add to PATH: export PATH=\"$install_dir:\$PATH\""
        fi

        # Setup shell wrapper
        setup_shell

        success "Installed!"

        success "Run 'cokacctl' to start."
    else
        error "Installation failed"
    fi
}

main "$@"
