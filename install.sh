#!/bin/bash
#
# COKACDIR Installer
# Usage: curl -fsSL https://cokacdir.cokac.com/install.sh | bash
#

set -e

BINARY_NAME="cokacdir"
BASE_URL="https://cokacdir.cokac.com/dist"
INSTALL_TMPFILE=""
INSTALL_STAGEFILE=""
INSTALL_STAGE_NEEDS_SUDO=0
NOTICE_TMPDIR=""

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

download_distribution_notices() {
    NOTICE_TMPDIR="$(mktemp -d)" || return 1
    mkdir -p "$NOTICE_TMPDIR/LICENSES" || return 1

    download "$BASE_URL/LICENSE" "$NOTICE_TMPDIR/LICENSE" || return 1
    download "$BASE_URL/THIRD_PARTY_NOTICES.md" \
        "$NOTICE_TMPDIR/THIRD_PARTY_NOTICES.md" || return 1
    download "$BASE_URL/LICENSES/OpenSSL-3.6.3.txt" \
        "$NOTICE_TMPDIR/LICENSES/OpenSSL-3.6.3.txt" || return 1

    local notice
    for notice in \
        "$NOTICE_TMPDIR/LICENSE" \
        "$NOTICE_TMPDIR/THIRD_PARTY_NOTICES.md" \
        "$NOTICE_TMPDIR/LICENSES/OpenSSL-3.6.3.txt"
    do
        [ -s "$notice" ] || return 1
    done

    grep -Fq "MIT License" "$NOTICE_TMPDIR/LICENSE" || return 1
    grep -Fq "# Third-Party Notices" \
        "$NOTICE_TMPDIR/THIRD_PARTY_NOTICES.md" || return 1
    grep -Fq "OpenSSL 3.6.3" \
        "$NOTICE_TMPDIR/THIRD_PARTY_NOTICES.md" || return 1
    grep -Fq "Apache License" \
        "$NOTICE_TMPDIR/LICENSES/OpenSSL-3.6.3.txt" || return 1
    grep -Fq "Version 2.0, January 2004" \
        "$NOTICE_TMPDIR/LICENSES/OpenSSL-3.6.3.txt" || return 1
}

get_notice_dir() {
    local install_dir="$1"
    if [ "$install_dir" = "$HOME/.local/bin" ]; then
        echo "$HOME/.local/share/doc/$BINARY_NAME"
    else
        echo "${install_dir%/bin}/share/doc/$BINARY_NAME"
    fi
}

publish_notice_file() {
    local source="$1"
    local destination="$2"
    local needs_sudo="$3"
    local parent base stage
    parent="$(dirname "$destination")"
    base="$(basename "$destination")"

    if [ -L "$destination" ] || { [ -e "$destination" ] && [ ! -f "$destination" ]; }; then
        return 1
    fi

    if [ "$needs_sudo" -eq 1 ]; then
        stage="$(sudo mktemp "$parent/.${base}.XXXXXX")" || return 1
        if ! sudo cp "$source" "$stage" || \
           ! sudo chmod 644 "$stage" || \
           ! sudo mv -f "$stage" "$destination"
        then
            sudo rm -f "$stage" >/dev/null 2>&1 || true
            return 1
        fi
    else
        stage="$(mktemp "$parent/.${base}.XXXXXX")" || return 1
        if ! cp "$source" "$stage" || \
           ! chmod 644 "$stage" || \
           ! mv -f "$stage" "$destination"
        then
            rm -f "$stage" 2>/dev/null || true
            return 1
        fi
    fi
}

install_distribution_notices() {
    local install_dir="$1"
    local notice_dir needs_sudo
    notice_dir="$(get_notice_dir "$install_dir")"
    needs_sudo=0

    if [ -L "$notice_dir" ] || [ -L "$notice_dir/LICENSES" ]; then
        return 1
    fi

    if mkdir -p "$notice_dir/LICENSES" 2>/dev/null && \
       [ -w "$notice_dir" ] && [ -w "$notice_dir/LICENSES" ]
    then
        needs_sudo=0
    else
        if ! has_cmd sudo || ! sudo mkdir -p "$notice_dir/LICENSES"; then
            return 1
        fi
        needs_sudo=1
    fi

    if [ "$needs_sudo" -eq 1 ]; then
        sudo chmod 755 "$notice_dir" "$notice_dir/LICENSES" || return 1
    else
        chmod 755 "$notice_dir" "$notice_dir/LICENSES" || return 1
    fi

    publish_notice_file "$NOTICE_TMPDIR/LICENSE" \
        "$notice_dir/LICENSE" "$needs_sudo" || return 1
    publish_notice_file "$NOTICE_TMPDIR/THIRD_PARTY_NOTICES.md" \
        "$notice_dir/THIRD_PARTY_NOTICES.md" "$needs_sudo" || return 1
    publish_notice_file "$NOTICE_TMPDIR/LICENSES/OpenSSL-3.6.3.txt" \
        "$notice_dir/LICENSES/OpenSSL-3.6.3.txt" "$needs_sudo" || return 1
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
    if [ -n "${NOTICE_TMPDIR:-}" ]; then
        rm -rf "$NOTICE_TMPDIR" 2>/dev/null || true
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
        if [ -n "$cokacdir_lastdir" ] && [ -d "$cokacdir_lastdir" ]; then
            cd -- "$cokacdir_lastdir" || cokacdir_status=$?
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

    info "Downloading license notices..."
    if ! download_distribution_notices; then
        error "Could not download required license notices"
    fi

    # Use a deterministic public executable mode. mktemp normally creates a
    # 0600 file, so chmod +x would install a root-owned 0700 system binary.
    chmod 755 "$INSTALL_TMPFILE"

    # Publish notices before the executable. If notice installation fails, an
    # existing executable remains untouched instead of being replaced by a
    # release whose accompanying license material is unavailable.
    if ! install_distribution_notices "$install_dir"; then
        error "Could not install required license notices"
    fi
    rm -rf "$NOTICE_TMPDIR"
    NOTICE_TMPDIR=""

    # Install. For a privileged system directory, first copy to a unique file
    # beside the destination and only then atomically rename it over the old
    # executable. A failed cross-filesystem copy cannot truncate the old file.
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

        success "Installed! Run 'cokacdir' to start."
    else
        error "Installation failed"
    fi
}

trap cleanup_install_files EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
main "$@"
