#!/bin/sh
# Lato installer: downloads a prebuilt binary from GitHub releases,
# verifies its SHA256 checksum, and installs it into ~/.local/bin.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/hyzwhu/lato/master/scripts/install.sh | sh
#   sh install.sh [version]        # e.g. v0.1.0-beta.2; defaults to the latest release
#
# Environment overrides:
#   LATO_INSTALL_DIR  target directory (default: ~/.local/bin)
set -eu

REPO="hyzwhu/lato"
INSTALL_DIR="${LATO_INSTALL_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Darwin) target="apple-darwin" ;;
    Linux) target="unknown-linux-gnu" ;;
    *)
        echo "error: unsupported OS '$os'; on Windows, download the zip manually from" >&2
        echo "  https://github.com/$REPO/releases/latest" >&2
        exit 1
        ;;
esac
case "$arch" in
    x86_64|amd64) arch_name="x86_64" ;;
    aarch64|arm64) arch_name="aarch64" ;;
    *)
        echo "error: unsupported architecture '$arch'" >&2
        exit 1
        ;;
esac
asset_target="${arch_name}-${target}"

if [ "$#" -ge 1 ]; then
    version="$1"
else
    echo "Determining latest release..."
    version="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')"
    if [ -z "$version" ]; then
        echo "error: could not determine the latest release; pass a version, e.g. $0 v0.1.0-beta.2" >&2
        exit 1
    fi
fi

case "$version" in
    v*) ;;
    *) version="v$version" ;;
esac

archive="lato-${version#v}-${asset_target}.tar.gz"
base_url="https://github.com/$REPO/releases/download/$version"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

echo "Downloading $archive..."
curl -fsSL -o "$tmp_dir/$archive" "$base_url/$archive"
curl -fsSL -o "$tmp_dir/SHA256SUMS" "$base_url/SHA256SUMS"

echo "Verifying checksum..."
expected="$(grep "$archive" "$tmp_dir/SHA256SUMS" | awk '{print $1}')"
if [ -z "$expected" ]; then
    echo "error: no checksum entry for $archive" >&2
    exit 1
fi
actual="$(sha256sum "$tmp_dir/$archive" | awk '{print $1}')"
if [ "$actual" != "$expected" ]; then
    echo "error: checksum mismatch for $archive (expected $expected, got $actual)" >&2
    exit 1
fi

tar -xzf "$tmp_dir/$archive" -C "$tmp_dir"
mkdir -p "$INSTALL_DIR"
mv "$tmp_dir/lato-${version#v}-${asset_target}/lato" "$INSTALL_DIR/lato"
chmod +x "$INSTALL_DIR/lato"

echo "Installed $version to $INSTALL_DIR/lato"
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
    echo ""
    echo "NOTE: $INSTALL_DIR is not on your PATH. Add this to your shell profile:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi
echo "Run 'lato doctor' to verify your setup."
