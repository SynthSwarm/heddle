#!/bin/sh
# Installs a released heddle binary.
#
#   curl -fsSL https://raw.githubusercontent.com/SynthSwarm/heddle/main/install.sh | sh
#
# POSIX sh rather than bash: the one-liner above runs under whatever /bin/sh is, which on
# Debian and Alpine is not bash. Nothing here needs arrays or [[.
#
# Re-running is safe. The binary is downloaded to a temporary directory, checksummed, and
# only then moved into place, so an interrupted run cannot leave a half-written heddle on
# PATH and a successful run over an existing install is a replacement rather than a
# second copy.

set -eu

REPO="SynthSwarm/heddle"
API="https://api.github.com/repos/${REPO}/releases"

# Overridable by flag or environment; the flags are parsed below and win.
VERSION="${HEDDLE_VERSION:-}"
INSTALL_DIR="${HEDDLE_INSTALL_DIR:-}"
TARGET="${HEDDLE_TARGET:-}"

usage() {
	cat <<'EOF'
Install heddle, an agent-native Matrix client for the terminal.

Usage: install.sh [options]

Options:
  --version VERSION   Version to install, e.g. 0.3.0 or v0.3.0.
                      Default: the most recent release.
  --dir DIR           Directory to install into.
                      Default: ~/.local/bin, or /usr/local/bin when running as root.
  --target TARGET     Release target triple to fetch, e.g. x86_64-unknown-linux-musl.
                      Default: detected from uname.
  --help              Show this message.

Environment:
  HEDDLE_VERSION, HEDDLE_INSTALL_DIR, HEDDLE_TARGET  Same as the flags above.
  GITHUB_TOKEN                                       Used for the API call if set,
                                                     which raises the rate limit.
EOF
}

say() { printf 'heddle: %s\n' "$*"; }
err() { printf 'heddle: %s\n' "$*" >&2; }
die() {
	err "$*"
	exit 1
}

need() {
	command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

while [ $# -gt 0 ]; do
	case "$1" in
	--version)
		[ $# -ge 2 ] || die "--version needs a value"
		VERSION="$2"
		shift 2
		;;
	--version=*)
		VERSION="${1#*=}"
		shift
		;;
	--dir)
		[ $# -ge 2 ] || die "--dir needs a value"
		INSTALL_DIR="$2"
		shift 2
		;;
	--dir=*)
		INSTALL_DIR="${1#*=}"
		shift
		;;
	--target)
		[ $# -ge 2 ] || die "--target needs a value"
		TARGET="$2"
		shift 2
		;;
	--target=*)
		TARGET="${1#*=}"
		shift
		;;
	--help | -h)
		usage
		exit 0
		;;
	*)
		err "unknown option: $1"
		usage >&2
		exit 2
		;;
	esac
done

need tar
need mktemp
need uname

# curl or wget, whichever is present. Alpine images routinely have only wget.
if command -v curl >/dev/null 2>&1; then
	fetch() { curl -fsSL --retry 3 --retry-delay 1 ${GITHUB_TOKEN:+-H "Authorization: Bearer ${GITHUB_TOKEN}"} -o "$2" -- "$1"; }
elif command -v wget >/dev/null 2>&1; then
	fetch() { wget -q ${GITHUB_TOKEN:+--header="Authorization: Bearer ${GITHUB_TOKEN}"} -O "$2" -- "$1"; }
else
	die "need curl or wget"
fi

# sha256sum on GNU userland, shasum on macOS. Verification is not optional: an unverified
# download over a redirect chain is a binary of unknown provenance.
if command -v sha256sum >/dev/null 2>&1; then
	checksum() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
	checksum() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
	die "need sha256sum or shasum to verify the download"
fi

# ---------------------------------------------------------------------------
# Target
# ---------------------------------------------------------------------------

# Only the triples the release workflow actually builds are listed. Guessing a name the
# release does not contain produces a 404 halfway through; naming the gap produces an
# instruction the reader can follow.
detect_target() {
	os="$(uname -s)"
	arch="$(uname -m)"
	case "${os}:${arch}" in
	# musl rather than gnu: statically linked, so it does not inherit the glibc
	# version of whatever runner built it. For a binary being dropped onto an
	# arbitrary machine that is the difference between running and not.
	Linux:x86_64 | Linux:amd64)
		echo "x86_64-unknown-linux-musl"
		;;
	*)
		err "no released binary for ${os} ${arch}."
		err "heddle currently publishes x86_64 Linux only. Build from source:"
		err "  git clone https://github.com/${REPO}"
		err "  cd heddle && cargo build --release"
		exit 1
		;;
	esac
}

[ -n "$TARGET" ] || TARGET="$(detect_target)"

# ---------------------------------------------------------------------------
# Version
# ---------------------------------------------------------------------------

# Deliberately not /releases/latest. That endpoint excludes prereleases, and every 0.x
# release of heddle is marked as one by the release workflow -- so it currently reports
# v0.2.0 as "latest" and would keep installing it long after v0.3.0 shipped. Take the
# newest entry from the list instead, skipping drafts.
resolve_latest() {
	tmp_json="${tmp}/releases.json"
	fetch "${API}?per_page=20" "$tmp_json" ||
		die "could not reach the GitHub release API"

	# No jq dependency: an install script that needs a JSON parser installed first is
	# not an install script.
	#
	# Records are split on "assets_url", which appears exactly once per release and
	# before both fields of interest, so each release is evaluated as a unit. Relying
	# instead on tag_name and draft arriving in a fixed order is what an earlier
	# version of this did, and it quietly resolved the second-newest release.
	tag="$(
		# shellcheck disable=SC2020 # set-to-set is intended: both delimiters become newlines
		tr ',{' '\n\n' <"$tmp_json" |
			sed -n 's/.*"assets_url".*/MARK/p;
			        s/.*"draft"[[:space:]]*:[[:space:]]*\(true\|false\).*/DRAFT \1/p;
			        s/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/TAG \1/p' |
			awk '
			  # `exit` inside a rule still runs END, so an unguarded flush()
			  # printed the tag twice and the caller captured both lines.
			  function flush() {
			    if (printed || !have || tag == "" || draft == "true") return
			    printed = 1
			    print tag
			  }
			  $1 == "MARK"  { flush(); if (printed) exit; have = 1; tag = ""; draft = ""; next }
			  $1 == "TAG"   { tag = $2; next }
			  $1 == "DRAFT" { draft = $2; next }
			  END           { flush() }
			'
	)"

	[ -n "$tag" ] || die "no published release found for ${REPO}"
	echo "$tag"
}

# ---------------------------------------------------------------------------
# Install directory
# ---------------------------------------------------------------------------

default_install_dir() {
	if [ "$(id -u)" -eq 0 ]; then
		echo "/usr/local/bin"
	else
		echo "${HOME}/.local/bin"
	fi
}

[ -n "$INSTALL_DIR" ] || INSTALL_DIR="$(default_install_dir)"

# ---------------------------------------------------------------------------
# Do the work
# ---------------------------------------------------------------------------

tmp="$(mktemp -d "${TMPDIR:-/tmp}/heddle-install.XXXXXX")"
# Covers the error paths too, which is the point -- `set -e` exits without unwinding.
# PIPE is listed because piping this script's output into something that exits early,
# `| head` being the obvious one, otherwise kills it before the temp directory is removed.
trap 'rm -rf "$tmp"' EXIT INT TERM HUP PIPE

if [ -n "$VERSION" ]; then
	# Accept 0.3.0 and v0.3.0 alike; the tag carries the v, the artefacts do not.
	case "$VERSION" in
	v*) tag="$VERSION" ;;
	*) tag="v${VERSION}" ;;
	esac
else
	say "resolving the latest release"
	tag="$(resolve_latest)"
fi

version="${tag#v}"
name="heddle-${version}-${TARGET}"
base="https://github.com/${REPO}/releases/download/${tag}"

say "installing ${tag} (${TARGET})"

fetch "${base}/${name}.tar.gz" "${tmp}/${name}.tar.gz" ||
	die "no artefact ${name}.tar.gz in release ${tag}"
fetch "${base}/${name}.tar.gz.sha256" "${tmp}/${name}.tar.gz.sha256" ||
	die "release ${tag} has no checksum for ${name}.tar.gz; refusing to install"

expected="$(cut -d' ' -f1 <"${tmp}/${name}.tar.gz.sha256")"
actual="$(checksum "${tmp}/${name}.tar.gz")"
if [ "$expected" != "$actual" ]; then
	err "checksum mismatch for ${name}.tar.gz"
	err "  expected ${expected}"
	err "  actual   ${actual}"
	exit 1
fi
say "checksum verified"

tar -C "$tmp" -xzf "${tmp}/${name}.tar.gz"
[ -f "${tmp}/${name}/heddle" ] || die "archive did not contain a heddle binary"

mkdir -p "$INSTALL_DIR" || die "could not create ${INSTALL_DIR}"
if [ ! -w "$INSTALL_DIR" ]; then
	err "no write permission for ${INSTALL_DIR}."
	err "Re-run with --dir DIR, or with sudo to install system-wide."
	exit 1
fi

chmod 755 "${tmp}/${name}/heddle"
# Same filesystem as the target, so this is a rename: either the old binary or the new
# one is on PATH, never a partial file. Replacing a running binary this way is also safe,
# where `cp` over it would not be.
mv "${tmp}/${name}/heddle" "${INSTALL_DIR}/heddle.tmp.$$"
mv "${INSTALL_DIR}/heddle.tmp.$$" "${INSTALL_DIR}/heddle"

say "installed ${INSTALL_DIR}/heddle"

# Being on PATH is the difference between installed and usable, and ~/.local/bin is
# frequently not. Report it rather than editing the user's shell profile behind them.
case ":${PATH}:" in
*":${INSTALL_DIR}:"*) ;;
*)
	say ""
	say "${INSTALL_DIR} is not on your PATH. Add it:"
	say "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.profile"
	;;
esac

say ""
say "Next: heddle --check      # verifies terminal, store and homeserver"
