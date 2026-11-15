#!/bin/sh
# MO installer for a generic Linux host.
# Copies prebuilt release binaries. It never builds.
# Real dirs: bin $HOME/.local/bin, unit $HOME/.config/systemd/user.
# Test mode: MO_INSTALL_PREFIX replaces $HOME for both dirs, and the
# installed unit is rewritten to the prefix paths.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DAEMON_SRC="$ROOT/target/release/mo-daemon"
CLI_SRC="$ROOT/target/release/mo-cli"
UNIT_SRC="$ROOT/systemd/mo-daemon-generic.service"

if [ ! -x "$DAEMON_SRC" ] || [ ! -x "$CLI_SRC" ]; then
    echo "install.sh: release binaries not found in $ROOT/target/release" >&2
    echo "Build them first:" >&2
    echo "  cargo build --release --features cli" >&2
    exit 1
fi
if [ ! -f "$UNIT_SRC" ]; then
    echo "install.sh: unit file not found: $UNIT_SRC" >&2
    exit 1
fi

PREFIX="${MO_INSTALL_PREFIX:-$HOME}"
BIN_DIR="$PREFIX/.local/bin"
UNIT_DIR="$PREFIX/.config/systemd/user"

if [ -f "$UNIT_DIR/mo-daemon.service" ]; then
    echo "install.sh: $UNIT_DIR/mo-daemon.service already exists." >&2
    echo "This host already runs MO under the chromebook unit name." >&2
    echo "Refusing to install a second unit for the same daemon paths." >&2
    exit 1
fi

mkdir -p "$BIN_DIR" "$UNIT_DIR"

# Upgrade path: keep the prior binaries before any overwrite.
if [ -x "$BIN_DIR/mo-daemon" ] || [ -x "$BIN_DIR/mo-cli" ]; then
    mkdir -p "$BIN_DIR/mo-backup"
    [ -x "$BIN_DIR/mo-daemon" ] && cp "$BIN_DIR/mo-daemon" "$BIN_DIR/mo-backup/mo-daemon"
    [ -x "$BIN_DIR/mo-cli" ] && cp "$BIN_DIR/mo-cli" "$BIN_DIR/mo-backup/mo-cli"
    echo "install.sh: prior binaries backed up to $BIN_DIR/mo-backup"
fi

# Replace by copy to a temp name plus an atomic rename.
for bin in mo-daemon mo-cli; do
    src="$ROOT/target/release/$bin"
    cp "$src" "$BIN_DIR/.$bin.tmp"
    chmod +x "$BIN_DIR/.$bin.tmp"
    mv "$BIN_DIR/.$bin.tmp" "$BIN_DIR/$bin"
done

if [ "$PREFIX" = "$HOME" ]; then
    cp "$UNIT_SRC" "$UNIT_DIR/mo-daemon-generic.service"
else
    sed "s|%h|$PREFIX|g" "$UNIT_SRC" > "$UNIT_DIR/mo-daemon-generic.service"
fi

echo "install.sh: installed $BIN_DIR/mo-daemon"
echo "install.sh: installed $BIN_DIR/mo-cli"
echo "install.sh: installed $UNIT_DIR/mo-daemon-generic.service"

if systemctl --user daemon-reload 2>/dev/null; then
    echo "Enable and start with:"
    echo "  systemctl --user enable --now mo-daemon-generic.service"
else
    echo "No systemd user manager was reached. The files are installed."
    echo "With systemd, enable and start with:"
    echo "  systemctl --user daemon-reload"
    echo "  systemctl --user enable --now mo-daemon-generic.service"
    echo "Without systemd, run the daemon directly:"
    echo "  MO_HOST=generic-linux $BIN_DIR/mo-daemon"
fi
