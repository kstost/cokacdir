#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
WEBSITE_DIR="$ROOT_DIR/website"
LOCK_DIR="$ROOT_DIR/.buildweb.lock"
LOCK_OWNED=0
STAGE_DIR=""
SITE_STAGE=""
INDEX_BACKUP=""
OG_BACKUP=""
PUBLISH_STARTED=0
INDEX_PUBLISHED=0
OG_PUBLISHED=0

cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    rollback_failed=0
    if [ "$status" -ne 0 ] && [ "$PUBLISH_STARTED" -eq 1 ]; then
        PUBLISH_STARTED=0
        rollback_path "$ROOT_DIR/index.html" "$INDEX_BACKUP" "$STAGE_DIR/failed-index.html" "$INDEX_PUBLISHED" || rollback_failed=1
        rollback_path "$ROOT_DIR/ogimg.jpg" "$OG_BACKUP" "$STAGE_DIR/failed-ogimg.jpg" "$OG_PUBLISHED" || rollback_failed=1
    fi
    if [ "$rollback_failed" -eq 1 ]; then
        echo "Web publish rollback was incomplete; recovery files are preserved at $STAGE_DIR" >&2
    elif [ -n "$STAGE_DIR" ]; then
        if ! rm -rf "$STAGE_DIR"; then
            echo "Could not remove web build staging directory: $STAGE_DIR" >&2
            if [ "$status" -eq 0 ]; then
                status=1
            fi
        fi
    fi
    if [ "$LOCK_OWNED" -eq 1 ]; then
        if ! rm -rf "$LOCK_DIR"; then
            echo "Could not remove web build lock: $LOCK_DIR" >&2
            if [ "$status" -eq 0 ]; then
                status=1
            fi
        fi
    fi
    exit "$status"
}

rollback_path() {
    target=$1
    backup=$2
    failed=$3
    published=$4

    if [ -e "$backup" ] || [ -L "$backup" ]; then
        if [ -e "$target" ] || [ -L "$target" ]; then
            if ! mv "$target" "$failed"; then
                echo "Could not preserve failed publish path: $target" >&2
                return 1
            fi
        fi
        if ! mv "$backup" "$target"; then
            echo "Could not restore backup $backup to $target" >&2
            return 1
        fi
    elif [ "$published" -eq 1 ] && { [ -e "$target" ] || [ -L "$target" ]; }; then
        if ! mv "$target" "$failed"; then
            echo "Could not remove newly published path safely: $target" >&2
            return 1
        fi
    fi
    return 0
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# npm and the root publication both use shared paths. Refuse a concurrent run
# instead of allowing two successful builds to publish a mixed asset/index set.
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
    echo "Another web build is already active (lock: $LOCK_DIR)." >&2
    echo "If no build is running, remove the stale lock directory manually." >&2
    exit 75
fi
LOCK_OWNED=1
printf '%s\n' "$$" > "$LOCK_DIR/pid"

STAGE_DIR="$(mktemp -d "$ROOT_DIR/.web-stage.XXXXXX")"
SITE_STAGE="$STAGE_DIR/site"
INDEX_BACKUP="$STAGE_DIR/previous-index.html"
OG_BACKUP="$STAGE_DIR/previous-ogimg.jpg"

cd "$WEBSITE_DIR"
npm ci
npm run build

mkdir -p "$SITE_STAGE"
cp -R "$WEBSITE_DIR/dist/." "$SITE_STAGE/"

test -s "$SITE_STAGE/index.html"
test -d "$SITE_STAGE/assets"
if [ -z "$(find "$SITE_STAGE/assets" -type f -print)" ]; then
    echo "Build produced no web assets" >&2
    exit 1
fi

if [ -L "$ROOT_DIR/assets" ]; then
    echo "Refusing to replace symlinked assets directory" >&2
    exit 1
fi
if [ -e "$ROOT_DIR/assets" ] && [ ! -d "$ROOT_DIR/assets" ]; then
    echo "Refusing to replace non-directory assets path" >&2
    exit 1
fi
for existing_file in "$ROOT_DIR/index.html" "$ROOT_DIR/ogimg.jpg"; do
    if [ -L "$existing_file" ]; then
        echo "Refusing to replace symlinked path: $existing_file" >&2
        exit 1
    fi
    if [ -e "$existing_file" ] && [ ! -f "$existing_file" ]; then
        echo "Refusing to replace non-regular path: $existing_file" >&2
        exit 1
    fi
done

PUBLISH_STARTED=1
if [ -f "$ROOT_DIR/index.html" ]; then
    cp -p "$ROOT_DIR/index.html" "$INDEX_BACKUP.tmp"
    mv "$INDEX_BACKUP.tmp" "$INDEX_BACKUP"
fi
if [ -f "$ROOT_DIR/ogimg.jpg" ]; then
    cp -p "$ROOT_DIR/ogimg.jpg" "$OG_BACKUP.tmp"
    mv "$OG_BACKUP.tmp" "$OG_BACKUP"
fi

# Vite assets are content-hashed. Install every new immutable asset while the
# previous assets remain available, then switch index.html last. Removing the
# old directory first creates both a publication window and a lasting failure
# for clients that fetched the previous index just before deployment.
if [ ! -d "$ROOT_DIR/assets" ]; then
    mkdir "$ROOT_DIR/assets"
fi
unexpected_root_assets=$(find "$ROOT_DIR/assets" \( -type d ! -path "$ROOT_DIR/assets" -o ! -type d ! -type f \) -print)
if [ -n "$unexpected_root_assets" ]; then
    echo "Refusing to publish into assets containing links or special files" >&2
    exit 1
fi
unexpected_stage_assets=$(find "$SITE_STAGE/assets" \( -type d ! -path "$SITE_STAGE/assets" -o ! -type d ! -type f \) -print)
if [ -n "$unexpected_stage_assets" ]; then
    echo "Refusing generated nested, linked, or special assets" >&2
    exit 1
fi
while IFS= read -r source; do
    relative=${source#"$SITE_STAGE/assets/"}
    case "$relative" in
        ""|*/*|*[!A-Za-z0-9._-]*)
            echo "Refusing unexpected generated asset name: $relative" >&2
            exit 1
            ;;
    esac
    destination="$ROOT_DIR/assets/$relative"
    if [ -e "$destination" ] || [ -L "$destination" ]; then
        if [ ! -L "$destination" ] && [ -f "$destination" ] && cmp -s "$source" "$destination"; then
            continue
        fi
        echo "Generated asset collides with different existing content: $relative" >&2
        exit 1
    fi
    # STAGE_DIR is deliberately created below ROOT_DIR, so this hard link is
    # same-filesystem and makes a complete asset visible in one namespace op.
    ln "$source" "$destination"
done < <(find "$SITE_STAGE/assets" -type f -print)
if [ -f "$SITE_STAGE/ogimg.jpg" ]; then
    OG_PUBLISHED=1
    mv -f "$SITE_STAGE/ogimg.jpg" "$ROOT_DIR/ogimg.jpg"
fi
# Publish index last so it never refers to assets that have not been installed.
INDEX_PUBLISHED=1
mv -f "$SITE_STAGE/index.html" "$ROOT_DIR/index.html"

PUBLISH_STARTED=0
