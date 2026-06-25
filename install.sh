#!/bin/sh
# install.sh — build a release `icode` and install it to a user-writable prefix.
#
# - No sudo. Installs to ${PREFIX:-$HOME/.local/bin}.
# - Idempotent: re-running rebuilds and overwrites the installed binary.
# - POSIX sh; `set -e` so any failed step aborts the install.
#
# Usage:
#   ./install.sh                 # installs to ~/.local/bin
#   PREFIX=/custom/bin ./install.sh
set -e

# Resolve the directory this script lives in, so the build runs against THIS
# repo regardless of the caller's cwd.
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PREFIX="${PREFIX:-$HOME/.local/bin}"
BIN_NAME="icode"
SRC_BIN="$SCRIPT_DIR/target/release/$BIN_NAME"
DEST_BIN="$PREFIX/$BIN_NAME"

echo "==> Building release binary (cargo build --release -p icode)"
( cd "$SCRIPT_DIR" && cargo build --release -p "$BIN_NAME" )

if [ ! -f "$SRC_BIN" ]; then
    echo "error: expected binary not found at $SRC_BIN" >&2
    exit 1
fi

echo "==> Installing to $DEST_BIN"
mkdir -p "$PREFIX"
# `cp` then `chmod` (rather than `install`, which isn't guaranteed on every
# minimal system) keeps this portable.
cp -f "$SRC_BIN" "$DEST_BIN"
chmod +x "$DEST_BIN"

echo ""
echo "Installed: $DEST_BIN"

# Remind the user about PATH only if the install dir isn't already on it. The
# `:$PATH:` wrapping makes the substring match exact (avoids false positives).
case ":$PATH:" in
    *":$PREFIX:"*)
        : # already on PATH, nothing to say
        ;;
    *)
        echo ""
        echo "NOTE: $PREFIX is not on your PATH. Add it, e.g.:"
        echo "    export PATH=\"$PREFIX:\$PATH\""
        echo "(put that in your ~/.profile or ~/.bashrc to make it permanent)"
        ;;
esac

echo ""
echo "Next step: run the onboarding wizard ->  icode setup [project_path]"
