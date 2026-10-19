#!/bin/sh
# Builds oxide and installs it for the current user: the binary and `ox` in ~/.local/bin,
# plus a desktop entry and icon so it appears in your app launcher.
#
#   scripts/install-linux.sh             build from source and install
#   scripts/install-linux.sh <dir>       install a prebuilt release folder instead
#   ./install-linux.sh                   (inside an unpacked release) install that release
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
prefix=${PREFIX:-$HOME/.local}

if [ $# -gt 0 ]; then
    binary="$1/oxide"
    share="$1"
elif [ -x "$(dirname "$0")/oxide" ]; then
    # Running from an unpacked release: everything sits next to this script.
    binary="$(dirname "$0")/oxide"
    share="$(dirname "$0")"
else
    cargo build --release --manifest-path "$root/Cargo.toml"
    binary="$root/target/release/oxide"
    share="$root/packaging/linux"
fi

install -Dm755 "$binary" "$prefix/bin/oxide"
ln -sf oxide "$prefix/bin/ox"
install -Dm644 "$share/oxide.desktop" "$prefix/share/applications/oxide.desktop"
install -Dm644 "$share/oxide.png" "$prefix/share/icons/hicolor/512x512/apps/oxide.png"
command -v update-desktop-database >/dev/null && update-desktop-database "$prefix/share/applications" || true

echo "Installed $prefix/bin/oxide (and ox)"
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "note: $prefix/bin is not on your PATH; add it to use \`ox\`" ;;
esac
