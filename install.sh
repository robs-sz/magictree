#!/bin/sh
# Install the latest magictree release for this machine.
#
#   curl -fsSL https://raw.githubusercontent.com/robs-sz/magictree/main/install.sh | sh
#
# It downloads the release archive built for this platform, checks that the
# binary inside runs, and puts it in ~/.local/bin. Nothing else is touched: no
# shell file is edited and nothing needs sudo.
#
#   MAGICTREE_INSTALL_DIR   where the binary goes (default: ~/.local/bin)
#   MAGICTREE_VERSION       a release to install instead of the latest, e.g. 0.2.0
#
# Afterwards `magictree update` keeps it current.

set -eu

repo="robs-sz/magictree"
dir="${MAGICTREE_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
  echo "magictree install: $1" >&2
  exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

os="$(uname -s)"
arch="$(uname -m)"
case "$os:$arch" in
  Darwin:arm64 | Darwin:aarch64) target="aarch64-apple-darwin" ;;
  Darwin:x86_64) target="x86_64-apple-darwin" ;;
  Linux:x86_64 | Linux:amd64) target="x86_64-unknown-linux-gnu" ;;
  *) fail "no release is built for $os on $arch; build it from source with
  cargo install --git https://github.com/$repo" ;;
esac

version="${MAGICTREE_VERSION:-}"
if [ -z "$version" ]; then
  # The releases page redirects `latest` to the newest tag, which answers the
  # question without the API and its rate limit.
  latest="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")" ||
    fail "could not reach github.com to find the latest release"
  version="${latest##*/v}"
  case "$version" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) fail "could not read the latest release from $latest" ;;
  esac
fi
version="${version#v}"

archive="magictree-$version-$target.tar.gz"
url="https://github.com/$repo/releases/download/v$version/$archive"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "downloading magictree $version for $target"
curl -fsSL "$url" -o "$tmp/$archive" || fail "could not download $url"
tar -xzf "$tmp/$archive" -C "$tmp" || fail "$archive is not a readable archive"
[ -f "$tmp/magictree" ] || fail "$archive holds no magictree binary"
chmod +x "$tmp/magictree"

# A binary that does not run here is never installed.
reported="$("$tmp/magictree" --version 2>/dev/null)" || fail "the downloaded binary does not run on this machine"
case "$reported" in
  *"$version"*) ;;
  *) fail "the downloaded binary reports '$reported', not $version" ;;
esac

mkdir -p "$dir" || fail "could not create $dir"
# Moved into place under its final name's directory, so a running magictree
# keeps its old file and the new one appears whole.
mv "$tmp/magictree" "$dir/.magictree.new" || fail "could not write to $dir"
mv "$dir/.magictree.new" "$dir/magictree" || fail "could not write to $dir"

echo "installed $reported to $dir/magictree"
case ":$PATH:" in
  *":$dir:"*) ;;
  *)
    echo
    echo "$dir is not on your PATH. Add it in your shell's rc file:"
    echo "  export PATH=\"$dir:\$PATH\""
    ;;
esac
echo
echo "next: run \`magictree discover && magictree init\` in a repository's primary checkout"
