#!/bin/sh
# Installs the latest oxide release for the current user on Linux:
#
#   curl -fsSL https://raw.githubusercontent.com/edkranz/oxide/main/scripts/install.sh | sh
#
# Puts `oxide` and `ox` in ~/.local/bin (PREFIX changes that) with a desktop entry.
# On macOS, use Homebrew: brew install --cask edkranz/tap/oxide
set -eu

case "$(uname -s)" in
    Linux) ;;
    Darwin)
        echo "On macOS, install with Homebrew: brew install --cask edkranz/tap/oxide" >&2
        exit 1
        ;;
    *)
        echo "This installer is for Linux. See https://github.com/edkranz/oxide/releases" >&2
        exit 1
        ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *)
        echo "No prebuilt oxide for $(uname -m); build it with cargo instead." >&2
        exit 1
        ;;
esac

url="https://github.com/edkranz/oxide/releases/latest/download/Oxide-linux-$arch.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading $url"
curl -fsSL "$url" | tar -xz -C "$tmp"
sh "$tmp/oxide/install-linux.sh"
