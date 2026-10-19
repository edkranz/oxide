#!/bin/sh
# Builds Oxide.app, installs it into /Applications, and links the `ox` command onto your PATH.
#
#   scripts/install-macos.sh                 build and install
#   scripts/install-macos.sh --bundle-only   just build target/release/Oxide.app
#
# APP_DIR and BIN_DIR override the install locations (default /Applications, ~/.local/bin).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
app_dir=${APP_DIR:-/Applications}
bin_dir=${BIN_DIR:-$HOME/.local/bin}
identifier=io.github.edkranz.oxide
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)

# OXIDE_SKIP_BUILD=1 packages an existing target/release/oxide (CI builds a universal one).
if [ -z "${OXIDE_SKIP_BUILD:-}" ]; then
    cargo build --release --manifest-path "$root/Cargo.toml"
fi

bundle="$root/target/release/Oxide.app"
rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources" "$bundle/Contents/SharedSupport/bin"
cp "$root/target/release/oxide" "$bundle/Contents/MacOS/oxide"
cp "$root/packaging/macos/ox" "$bundle/Contents/SharedSupport/bin/ox"
cp "$root/packaging/macos/Oxide.icns" "$bundle/Contents/Resources/Oxide.icns"
sed "s/__VERSION__/$version/g" "$root/packaging/macos/Info.plist" > "$bundle/Contents/Info.plist"
# An ad-hoc signature is enough for a locally built app to launch on Apple silicon.
codesign --force --sign - "$bundle"

if [ "${1:-}" = "--bundle-only" ]; then
    echo "Built $bundle"
    exit 0
fi

target="$app_dir/Oxide.app"
if [ -e "$target" ]; then
    existing=$(defaults read "$target/Contents/Info" CFBundleIdentifier 2>/dev/null || true)
    if [ "$existing" != "$identifier" ]; then
        echo "error: $target exists and is not this Oxide ($existing); not replacing it" >&2
        exit 1
    fi
    rm -rf "$target"
fi
ditto "$bundle" "$target"

mkdir -p "$bin_dir"
ln -sf "$target/Contents/SharedSupport/bin/ox" "$bin_dir/ox"

echo "Installed $target"
echo "Linked $bin_dir/ox -> $target/Contents/SharedSupport/bin/ox"
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "note: $bin_dir is not on your PATH; add it to use \`ox\`" ;;
esac
