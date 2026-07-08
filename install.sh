#!/bin/sh
# audible-rs installer — download a prebuilt `audible` binary from GitHub
# Releases and install it. POSIX sh; needs only base tools (curl/wget, tar,
# sha256sum/shasum/openssl), which stock Linux and macOS already ship.
#
#   curl -fsSL https://raw.githubusercontent.com/mkb79/audible-rs/main/install.sh | sh
#
# Options (flag or environment variable):
#   --version <tag>   AUDIBLE_VERSION      release to install (default: newest, incl. pre-releases)
#   --bin-dir <dir>   AUDIBLE_INSTALL_DIR  install location (default: ~/.local/bin)
#
# Integrity: the download is verified against the release's SHA256SUMS over
# HTTPS. (Cryptographic signatures are a planned addition — see AUD-141.)

set -eu

REPO="mkb79/audible-rs"
BIN="audible"

VERSION="${AUDIBLE_VERSION:-}"
INSTALL_DIR="${AUDIBLE_INSTALL_DIR:-${HOME}/.local/bin}"

while [ $# -gt 0 ]; do
	case "$1" in
		--version) VERSION="${2:?--version needs a value}"; shift 2 ;;
		--bin-dir) INSTALL_DIR="${2:?--bin-dir needs a value}"; shift 2 ;;
		-h|--help) grep '^#' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' | head -n 14; exit 0 ;;
		*) echo "unknown option: $1" >&2; exit 1 ;;
	esac
done

err() { echo "error: $*" >&2; exit 1; }
info() { echo "$*" >&2; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- downloader -----------------------------------------------------------
if have curl; then
	dl()  { curl -fsSL "$1"; }
	dlo() { curl -fsSL -o "$1" "$2"; }
elif have wget; then
	dl()  { wget -qO- "$1"; }
	dlo() { wget -qO "$1" "$2"; }
else
	err "need curl or wget"
fi
have tar || err "need tar"

# --- detect the target triple --------------------------------------------
case "$(uname -s)" in
	Linux)  os="unknown-linux-musl" ;;
	Darwin) os="apple-darwin" ;;
	*) err "unsupported OS $(uname -s) — Linux and macOS only" ;;
esac
case "$(uname -m)" in
	x86_64|amd64)  arch="x86_64" ;;
	arm64|aarch64) arch="aarch64" ;;
	*) err "unsupported architecture $(uname -m)" ;;
esac
target="${arch}-${os}"

# --- resolve the version (newest, including pre-releases, when unset) -----
if [ -z "$VERSION" ]; then
	info "resolving the latest release…"
	VERSION="$(dl "https://api.github.com/repos/${REPO}/releases" \
		| grep '"tag_name":' | head -n 1 \
		| sed -e 's/.*"tag_name":[[:space:]]*"//' -e 's/".*//')"
	[ -n "$VERSION" ] || err "could not determine the latest release (pass --version <tag>)"
fi
num="${VERSION#v}"
archive="${BIN}-${num}-${target}.tar.gz"
base="https://github.com/${REPO}/releases/download/${VERSION}"

info "installing ${BIN} ${VERSION} (${target}) into ${INSTALL_DIR}"

# --- download to a temp dir ----------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM
dlo "${tmp}/${archive}" "${base}/${archive}" || err "download failed: ${base}/${archive}"
dlo "${tmp}/SHA256SUMS" "${base}/SHA256SUMS" || err "could not fetch SHA256SUMS"

# --- verify the SHA256 checksum ------------------------------------------
expected="$(awk -v f="$archive" '$2 == f { print $1 }' "${tmp}/SHA256SUMS")"
[ -n "$expected" ] || err "no checksum for ${archive} in SHA256SUMS"
if have sha256sum; then
	actual="$(sha256sum "${tmp}/${archive}" | awk '{print $1}')"
elif have shasum; then
	actual="$(shasum -a 256 "${tmp}/${archive}" | awk '{print $1}')"
elif have openssl; then
	actual="$(openssl dgst -sha256 "${tmp}/${archive}" | awk '{print $NF}')"
else
	err "need sha256sum, shasum or openssl to verify the download"
fi
[ "$actual" = "$expected" ] || err "checksum mismatch for ${archive}"
info "checksum verified"

# --- install --------------------------------------------------------------
tar -xzf "${tmp}/${archive}" -C "$tmp"
stage="${BIN}-${num}-${target}"
[ -f "${tmp}/${stage}/${BIN}" ] || err "unexpected archive layout (${stage}/${BIN} missing)"
mkdir -p "$INSTALL_DIR"
if have install; then
	install -m 0755 "${tmp}/${stage}/${BIN}" "${INSTALL_DIR}/${BIN}"
else
	cp "${tmp}/${stage}/${BIN}" "${INSTALL_DIR}/${BIN}" && chmod 0755 "${INSTALL_DIR}/${BIN}"
fi
info "installed ${INSTALL_DIR}/${BIN}"

# --- PATH hint ------------------------------------------------------------
case ":${PATH}:" in
	*":${INSTALL_DIR}:"*) : ;;
	*)
		info ""
		info "note: ${INSTALL_DIR} is not on your PATH. Add it to your shell profile:"
		info "  export PATH=\"${INSTALL_DIR}:\$PATH\""
		;;
esac

# --- optional decrypt tools ----------------------------------------------
info ""
info "Optional: 'audible download --decrypt' needs one of:"
info "  * ffmpeg (>= 4.4), or"
info "  * aaxclean-cli by Mbucari (purpose-built, faster): https://github.com/Mbucari/aaxclean-cli"
info "Point at a specific binary with AUDIBLE_FFMPEG / AUDIBLE_AAXCLEAN_CLI."
