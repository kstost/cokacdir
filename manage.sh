#!/bin/bash
#
# COKACCTL Installer
# Usage: curl -fsSL https://raw.githubusercontent.com/kstost/cokacctl/refs/heads/main/manage.sh | bash
#

set -e

BINARY_NAME="cokacctl"
BASE_URL="https://raw.githubusercontent.com/kstost/cokacctl/refs/heads/main/dist_beta"
INSTALL_TMPFILE=""
INSTALL_STAGEFILE=""
INSTALL_STAGE_NEEDS_SUDO=0

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
    # Prefer the system directory only when we can actually install there.
    if [ -d "/usr/local/bin" ] && { [ -w "/usr/local/bin" ] || command -v sudo >/dev/null 2>&1; }; then
        echo "/usr/local/bin"
    else
        # Fallback to ~/.local/bin
        mkdir -p "$HOME/.local/bin" || return $?
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

validate_binary() {
    local file="$1"
    local os="$2"
    local magic
    magic="$(LC_ALL=C od -An -tx1 -N4 "$file" 2>/dev/null | tr -d ' \n')"
    case "$os:$magic" in
        linux:7f454c46) return 0 ;;
        macos:feedface|macos:cefaedfe|macos:feedfacf|macos:cffaedfe|\
        macos:cafebabe|macos:bebafeca|macos:cafebabf|macos:bfbafeca) return 0 ;;
        *) return 1 ;;
    esac
}

cleanup_install_files() {
    if [ -n "${INSTALL_TMPFILE:-}" ]; then
        rm -f "$INSTALL_TMPFILE" 2>/dev/null || true
    fi
    if [ -n "${INSTALL_STAGEFILE:-}" ]; then
        if [ "${INSTALL_STAGE_NEEDS_SUDO:-0}" -eq 1 ]; then
            sudo rm -f "$INSTALL_STAGEFILE" >/dev/null 2>&1 || true
        else
            rm -f "$INSTALL_STAGEFILE" 2>/dev/null || true
        fi
    fi
}

# Canonical shell wrapper. The fixed lastdir file is a legacy cokacctl
# contract, so remove it before every invocation and only consume a fresh,
# valid directory written by that invocation.
write_shell_wrapper() {
    cat <<'COKACCTL_SHELL_WRAPPER'
# BEGIN COKACCTL SAFE SHELL WRAPPER
cokacctl() {
    local cokacctl_state_dir="$HOME/.cokacctl"
    local cokacctl_lastdir_file="$cokacctl_state_dir/lastdir"
    local cokacctl_lock_dir="$cokacctl_state_dir/.lastdir.lock"
    local cokacctl_status
    local cokacctl_lastdir
    local cokacctl_lock_owner

    mkdir -p "$cokacctl_state_dir" || return $?
    chmod 700 "$cokacctl_state_dir" 2>/dev/null || true

    if ! mkdir "$cokacctl_lock_dir" 2>/dev/null; then
        cokacctl_lock_owner="$(cat "$cokacctl_lock_dir/pid" 2>/dev/null || true)"
        if [ -n "$cokacctl_lock_owner" ] && ! kill -0 "$cokacctl_lock_owner" 2>/dev/null; then
            rm -rf "$cokacctl_lock_dir"
            mkdir "$cokacctl_lock_dir" 2>/dev/null || {
                echo "cokacctl: another invocation owns the last-directory state" >&2
                return 75
            }
        else
            echo "cokacctl: another invocation owns the last-directory state" >&2
            return 75
        fi
    fi
    printf '%s\n' "${BASHPID:-$$}" > "$cokacctl_lock_dir/pid" || {
        rm -rf "$cokacctl_lock_dir"
        return 1
    }
    if rm -f "$cokacctl_lastdir_file"; then
        :
    else
        cokacctl_status=$?
        rm -rf "$cokacctl_lock_dir"
        return "$cokacctl_status"
    fi

    if command cokacctl "$@"; then
        cokacctl_status=0
    else
        cokacctl_status=$?
    fi

    if [ "$cokacctl_status" -eq 0 ] && [ -s "$cokacctl_lastdir_file" ]; then
        cokacctl_lastdir="$(cat "$cokacctl_lastdir_file" 2>/dev/null)" || cokacctl_lastdir=""
        if [ -n "$cokacctl_lastdir" ] && [ -d "$cokacctl_lastdir" ]; then
            cd -- "$cokacctl_lastdir" || cokacctl_status=$?
        fi
    fi
    rm -f "$cokacctl_lastdir_file"
    rm -rf "$cokacctl_lock_dir"
    return "$cokacctl_status"
}
# END COKACCTL SAFE SHELL WRAPPER
COKACCTL_SHELL_WRAPPER
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

    # Append the repaired definition after any legacy function so re-running
    # the installer upgrades existing shell profiles.
    if [ -f "$config_file" ] && grep -Fq "BEGIN COKACCTL SAFE SHELL WRAPPER" "$config_file"; then
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

    info "Downloading cokacctl ($os-$arch)..."

    # Build download URL
    local filename="${BINARY_NAME}-${os}-${arch}"
    local url="${BASE_URL}/${filename}"

    # Select the destination before downloading so writable installations can
    # stage on the same filesystem and publish with one atomic rename.
    local install_dir
    install_dir="$(get_install_dir)" || error "Could not create an install directory"
    local install_path="${install_dir}/${BINARY_NAME}"
    if [ -L "$install_path" ] || { [ -e "$install_path" ] && [ ! -f "$install_path" ]; }; then
        error "Refusing to replace non-regular install path: $install_path"
    fi

    if [ -w "$install_dir" ]; then
        INSTALL_TMPFILE="$(mktemp "$install_dir/.${BINARY_NAME}.XXXXXX")"
    else
        INSTALL_TMPFILE="$(mktemp)"
    fi

    # Download
    if ! download "$url" "$INSTALL_TMPFILE"; then
        error "Download failed"
    fi
    if [ ! -s "$INSTALL_TMPFILE" ]; then
        error "Downloaded binary is empty"
    fi
    if ! validate_binary "$INSTALL_TMPFILE" "$os"; then
        error "Downloaded file is not a valid $os executable"
    fi

    chmod 755 "$INSTALL_TMPFILE"

    # Stage privileged installs beside the destination before the atomic
    # rename, so a failed /tmp-to-system copy cannot damage the old binary.
    if [ -w "$install_dir" ]; then
        mv -f "$INSTALL_TMPFILE" "$install_path"
        INSTALL_TMPFILE=""
    else
        INSTALL_STAGEFILE="$install_dir/.${BINARY_NAME}.$(basename "$INSTALL_TMPFILE").tmp"
        INSTALL_STAGE_NEEDS_SUDO=1
        if ! sudo cp "$INSTALL_TMPFILE" "$INSTALL_STAGEFILE"; then
            error "Could not stage the downloaded binary"
        fi
        if ! sudo chmod 755 "$INSTALL_STAGEFILE"; then
            error "Could not set executable permissions"
        fi
        if ! sudo mv -f "$INSTALL_STAGEFILE" "$install_path"; then
            error "Could not publish the downloaded binary"
        fi
        INSTALL_STAGEFILE=""
        rm -f "$INSTALL_TMPFILE"
        INSTALL_TMPFILE=""
    fi

    # Verify installation
    if [ -x "$install_path" ]; then
        # Check if in PATH
        case ":$PATH:" in
            *":$install_dir:"*) ;;
            *) warn "Add to PATH: export PATH=\"$install_dir:\$PATH\"" ;;
        esac

        # Setup shell wrapper
        setup_shell

        success "Installed!"

        success "Run 'cokacctl' to start."
    else
        error "Installation failed"
    fi
}

trap cleanup_install_files EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
main "$@"
