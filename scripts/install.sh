#!/usr/bin/env sh
# Install Arc from a GitHub release.
#
#   sh install.sh                 # latest release, into ~/.local/bin
#   sh install.sh v1.0.0          # a specific version
#   ARC_INSTALL_DIR=/opt/bin sh install.sh
#
# Download it, read it, then run it. Nothing here needs root, and nothing here
# should be piped straight into a root shell.
set -eu

REPO=martin-k-m/arc
VERSION=${1:-latest}
DIR=${ARC_INSTALL_DIR:-$HOME/.local/bin}

fail() { echo "arc install: $*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar  >/dev/null 2>&1 || fail "tar is required"

case "$(uname -s)" in
  Linux)  os=unknown-linux-gnu ;;
  Darwin) os=apple-darwin ;;
  *) fail "unsupported operating system $(uname -s); see the README for what is supported" ;;
esac

case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) fail "unsupported architecture $(uname -m)" ;;
esac

target="$arch-$os"
[ "$target" = "aarch64-unknown-linux-gnu" ] || [ "$target" = "x86_64-unknown-linux-gnu" ] ||
[ "$target" = "aarch64-apple-darwin" ] || [ "$target" = "x86_64-apple-darwin" ] ||
  fail "no release is published for $target"

if [ "$VERSION" = latest ]; then
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
            sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
  [ -n "$VERSION" ] || fail "could not determine the latest release"
fi

name="arc-$VERSION-$target"
base="https://github.com/$REPO/releases/download/$VERSION"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "arc install: downloading $name"
curl -fsSL --proto '=https' --tlsv1.2 -o "$tmp/$name.tar.gz" "$base/$name.tar.gz" ||
  fail "download failed; is $VERSION a published release?"
curl -fsSL --proto '=https' --tlsv1.2 -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" ||
  fail "could not download SHA256SUMS"

# An archive whose checksum has not been verified is not installed. If no
# checker is available that is a hard failure, not a skipped step.
if command -v sha256sum >/dev/null 2>&1; then
  sum=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
  sum=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)
else
  fail "neither sha256sum nor shasum is available, so the download cannot be verified"
fi
grep -q "^$sum  *$name.tar.gz\$" "$tmp/SHA256SUMS" ||
  fail "checksum mismatch for $name.tar.gz — refusing to install"
echo "arc install: checksum verified"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$DIR"
for b in arc arc-cache arc-worker; do
  install -m 755 "$tmp/$name/$b" "$DIR/$b"
done

echo "arc install: installed to $DIR"
case ":$PATH:" in
  *":$DIR:"*) "$DIR/arc" --version ;;
  *) echo "arc install: $DIR is not on PATH; add it, then run: arc --version" ;;
esac
