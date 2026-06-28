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

# Canonical shell wrapper. cokacctl extracts this marked block from install.sh
# so this file remains the source of truth for the user-facing cokacdir() shell
# function.
write_shell_wrapper() {
    cat <<'COKACDIR_SHELL_WRAPPER'
# BEGIN COKACDIR SHELL WRAPPER
# cokacdir - cd to last directory on interactive exit
cokacdir() {
    local cokacdir_lastdir_dir="$HOME/.cokacdir/_lastdir"
    local cokacdir_lastdir_file
    mkdir -p "$cokacdir_lastdir_dir" || return $?
    chmod 700 "$cokacdir_lastdir_dir" 2>/dev/null || true
    cokacdir_lastdir_file="$(mktemp "$cokacdir_lastdir_dir/cokacdir-lastdir.XXXXXX")" || return $?

    COKACDIR_LASTDIR_FILE="$cokacdir_lastdir_file" command cokacdir "$@"
    local cokacdir_status=$?

    if [ "$cokacdir_status" -eq 0 ] && [ -s "$cokacdir_lastdir_file" ]; then
        local cokacdir_lastdir
        cokacdir_lastdir="$(cat "$cokacdir_lastdir_file" 2>/dev/null)" || cokacdir_lastdir=""
        if [ -n "$cokacdir_lastdir" ]; then
            cd "$cokacdir_lastdir"
        fi
    fi

    rm -f "$cokacdir_lastdir_file"

    return "$cokacdir_status"
}
# END COKACDIR SHELL WRAPPER
COKACDIR_SHELL_WRAPPER
}

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

    # Check if already configured. Older installers added wrappers that changed
    # directory after commands like `cokacdir --version`; append the fixed wrapper
    # after those old definitions so re-running the installer corrects them.
    if [ -f "$config_file" ] && grep -q "cokacdir()" "$config_file"; then
        if grep -Fq "COKACDIR_LASTDIR_FILE=" "$config_file"; then
            return
        fi
        if grep -Fq 'command cokacdir "$@" && cd "$(cat ~/.cokacdir/lastdir' "$config_file" || \
           grep -Fq "local cokacdir_should_cd=1" "$config_file"; then
            echo "" >> "$config_file"
            write_shell_wrapper >> "$config_file"
        fi
        return
    fi

    # Create file if not exists
    if [ ! -f "$config_file" ]; then
        touch "$config_file"
    fi

    # Add function
    echo "" >> "$config_file"
    write_shell_wrapper >> "$config_file"
}

main() {
    # Detect platform
    local os arch
    os="$(detect_os)"
    arch="$(detect_arch)"

    info "Downloading cokacdir ($os-$arch)..."

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

        success "Installed! Run 'cokacdir' to start."
    else
        error "Installation failed"
    fi
}

main "$@"
