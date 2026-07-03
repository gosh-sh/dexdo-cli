#!/bin/sh
# dexdo installer for Linux and macOS.
#
#   curl -fsSL https://github.com/gosh-sh/dexdo-cli/releases/latest/download/install.sh | sh
#
# Detects your OS and CPU, downloads the matching release archive, verifies its
# checksum, and installs `dexdo` into ~/.local/bin (override with DEXDO_BIN_DIR).
set -eu

REPO="gosh-sh/dexdo-cli"
BINDIR="${DEXDO_BIN_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  osname="linux" ;;
  Darwin) osname="macos" ;;
  *) echo "dexdo: unsupported operating system: $os" >&2; exit 1 ;;
esac
case "$arch" in
  x86_64|amd64)   archname="x86_64" ;;
  aarch64|arm64)  archname="aarch64" ;;
  *) echo "dexdo: unsupported architecture: $arch" >&2; exit 1 ;;
esac

need() { command -v "$1" >/dev/null 2>&1 || { echo "dexdo: '$1' is required" >&2; exit 1; }; }
need curl
need tar

# Resolve the latest release tag.
ver="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
[ -n "$ver" ] || { echo "dexdo: could not resolve the latest release" >&2; exit 1; }
vern="${ver#v}"

asset="dexdo-${vern}-${archname}-${osname}.tar.gz"
base="https://github.com/${REPO}/releases/download/${ver}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "dexdo: downloading ${asset} (${ver})"
curl -fsSL "${base}/${asset}" -o "${tmp}/${asset}"

# Verify the archive checksum against SHA256SUMS. Fail closed: a missing
# SHA256SUMS or a missing entry for this asset aborts the install.
curl -fsSL "${base}/SHA256SUMS" -o "${tmp}/SHA256SUMS" || { echo "dexdo: could not fetch SHA256SUMS" >&2; exit 1; }
expected="$(grep " ${asset}\$" "${tmp}/SHA256SUMS" | awk '{print $1}' | head -n1)"
[ -n "$expected" ] || { echo "dexdo: ${asset} not found in SHA256SUMS" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${tmp}/${asset}" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "${tmp}/${asset}" | awk '{print $1}')"
fi
[ "$expected" = "$actual" ] || { echo "dexdo: checksum mismatch" >&2; exit 1; }
echo "dexdo: checksum verified"

tar -C "$tmp" -xzf "${tmp}/${asset}"
mkdir -p "$BINDIR"
install -m 0755 "${tmp}/dexdo-${vern}-${archname}-${osname}/dexdo" "${BINDIR}/dexdo"
echo "dexdo: installed ${ver} to ${BINDIR}/dexdo"

case ":${PATH}:" in
  *":${BINDIR}:"*) ;;
  *) echo "dexdo: add ${BINDIR} to your PATH to run 'dexdo'" ;;
esac
