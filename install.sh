#!/bin/sh
# Installs the cliproxy binary from a GitHub release, on Linux or macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.sh | sh
#
# It downloads the archive for this machine and the release's SHA256SUMS, checks the
# archive against it, and copies the binary into place. It does not write a config or
# start anything; see docs/GETTING-STARTED.md for that.
#
# Environment:
#   CLIPROXY_VERSION      release tag to install, such as v0.1.0 (default: the latest)
#   CLIPROXY_INSTALL_DIR  where the binary goes (default: $HOME/.local/bin)
#   CLIPROXY_RELEASES     release page base URL, for mirrors and tests
#                         (default: https://github.com/vayungodara/cliproxy-rs/releases)
set -eu

releases="${CLIPROXY_RELEASES:-https://github.com/vayungodara/cliproxy-rs/releases}"
dir="${CLIPROXY_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
  printf 'install.sh: %s\n' "$1" >&2
  exit 1
}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64 | Linux-amd64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) fail "there is no release binary for $(uname -s) $(uname -m); build from source (docs/INSTALL.md)" ;;
esac

tag="${CLIPROXY_VERSION:-}"
if [ -z "$tag" ]; then
  # /releases/latest redirects to /releases/tag/<tag> once a release exists.
  latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$releases/latest") || fail "cannot reach $releases"
  tag=${latest##*/tag/}
  [ "$tag" != "$latest" ] || fail "no release is published yet; build from source (docs/INSTALL.md)"
fi
name="cliproxy-${tag#v}-$target"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/$name.tar.gz" "$releases/download/$tag/$name.tar.gz" || fail "could not download $name.tar.gz from release $tag"
curl -fsSL -o "$tmp/SHA256SUMS" "$releases/download/$tag/SHA256SUMS" || fail "could not download SHA256SUMS from release $tag"

expected=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "$name.tar.gz is not listed in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$name.tar.gz" | awk '{ print $1 }')
else
  actual=$(shasum -a 256 "$tmp/$name.tar.gz" | awk '{ print $1 }')
fi
[ "$actual" = "$expected" ] || fail "checksum mismatch for $name.tar.gz; nothing was installed"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
install -m 0755 "$tmp/$name/cliproxy" "$dir/cliproxy"
if [ "$(uname -s)" = Darwin ]; then
  xattr -d com.apple.quarantine "$dir/cliproxy" 2>/dev/null || true
fi
printf 'Installed %s at %s/cliproxy\n' "$("$dir/cliproxy" --version 2>/dev/null | tail -n 1)" "$dir"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) printf 'Add %s to your PATH to run cliproxy by name.\n' "$dir" ;;
esac
