#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
rm -rf "$ROOT_DIR/builder/tools" "$ROOT_DIR/dist_beta" "$ROOT_DIR/target"
