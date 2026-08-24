#!/bin/sh
# Velos installer.
#
# Downloads a published release tarball for this machine's platform, verifies
# its SHA-256 checksum, and installs the requested binaries into a bin dir.
#
#   curl -fsSL https://raw.githubusercontent.com/blossomstack/velos/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/blossomstack/velos/main/install.sh | sh -s -- --components all
#
# Fails closed: an unsupported platform, a missing checksum, a checksum
# mismatch, or a binary that will not run on this machine all abort the install
# rather than leaving something half-working on your PATH.
#
# VELOS_DOWNLOAD_BASE overrides where release assets are fetched from (a mirror,
# or a local server in scripts/test-install.sh).

set -eu

REPO="blossomstack/velos"
KNOWN_COMPONENTS="velosctl veloslet velos-server"
DEFAULT_COMPONENTS="velosctl"
DEFAULT_BIN_DIR="${HOME}/.velos/bin"

version="${VELOS_VERSION:-}"
bin_dir="${VELOS_BIN_DIR:-$DEFAULT_BIN_DIR}"
components="${VELOS_COMPONENTS:-$DEFAULT_COMPONENTS}"
download_base="${VELOS_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download}"
sha_tool=""

say() { printf 'velos: %s\n' "$*"; }
die() { printf 'velos: error: %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<'USAGE'
Install the Velos binaries from a GitHub release.

Usage: install.sh [options]

Options:
  --components <list>  comma-separated: velosctl, veloslet, velos-server,
                       or all  (default: velosctl)
  --bin-dir <dir>      where to install  (default: ~/.velos/bin)
  --version <tag>      release tag to install, e.g. v0.1.3  (default: latest)
  -h, --help           show this help

Environment equivalents: VELOS_COMPONENTS, VELOS_BIN_DIR, VELOS_VERSION.
USAGE
}

# Verify the tarball against its published checksum file. Run from the
# directory holding both, since the checksum file names the file relatively.
sha_verify() {
    case "$sha_tool" in
        sha256sum) sha256sum -c "$1" ;;
        shasum) shasum -a 256 -c "$1" ;;
        *) return 1 ;;
    esac
}

# --------------------------------------------------------------------------
# Argument parsing
# --------------------------------------------------------------------------

while [ $# -gt 0 ]; do
    case "$1" in
        --components) [ $# -ge 2 ] || die "--components needs a value"; components="$2"; shift 2 ;;
        --components=*) components="${1#*=}"; shift ;;
        --bin-dir) [ $# -ge 2 ] || die "--bin-dir needs a value"; bin_dir="$2"; shift 2 ;;
        --bin-dir=*) bin_dir="${1#*=}"; shift ;;
        --version) [ $# -ge 2 ] || die "--version needs a value"; version="$2"; shift 2 ;;
        --version=*) version="${1#*=}"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

# --------------------------------------------------------------------------
# Preflight: tools, component names, platform
# --------------------------------------------------------------------------

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

if command -v sha256sum >/dev/null 2>&1; then
    sha_tool="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
    sha_tool="shasum"
else
    die "sha256sum or shasum is required to verify the download"
fi

# Normalize the component list: commas to spaces, `all` to every component, and
# reject anything we don't publish rather than 404ing on it later.
selected=""
if [ "$components" = "all" ]; then
    selected="$KNOWN_COMPONENTS"
else
    for want in $(printf '%s' "$components" | tr ',' ' '); do
        found=""
        for known in $KNOWN_COMPONENTS; do
            [ "$want" = "$known" ] && found="yes"
        done
        [ -n "$found" ] || die "unknown component '$want' (known: $KNOWN_COMPONENTS, or all)"
        selected="$selected $want"
    done
fi
[ -n "$selected" ] || die "no components selected"

# Map uname output onto the Rust target triples the release builds for.
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux)
        # Only glibc builds are published. A musl host would install cleanly and
        # then fail at exec time with an opaque loader error, so stop here.
        if [ -f /etc/alpine-release ] || ldd --version 2>&1 | grep -qi musl; then
            die "musl-based Linux has no published build; install from source with 'cargo install velosctl'"
        fi
        os_part="unknown-linux-gnu"
        ;;
    *) die "unsupported operating system: $os (supported: macOS, Linux)" ;;
esac
case "$arch" in
    arm64|aarch64) arch_part="aarch64" ;;
    x86_64|amd64) arch_part="x86_64" ;;
    *) die "unsupported architecture: $arch (supported: arm64, x86_64)" ;;
esac
target="${arch_part}-${os_part}"

# --------------------------------------------------------------------------
# Resolve and download the release
# --------------------------------------------------------------------------

if [ -z "$version" ]; then
    # /releases/latest redirects to /releases/tag/<version>; read the tag off the
    # final URL so we don't spend the API's anonymous rate limit.
    resolved="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
        "https://github.com/$REPO/releases/latest" 2>/dev/null || true)"
    version="${resolved##*/}"
    case "$version" in
        v*) : ;;
        *) die "could not determine the latest release; pass --version <tag>" ;;
    esac
fi

tarball="velos-${version}-${target}.tar.gz"
base_url="$download_base/$version"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "installing$selected ($version, $target)"

curl -fsSL -o "$tmp/$tarball" "$base_url/$tarball" \
    || die "download failed: $base_url/$tarball (is $version released for $target?)"
# -S is omitted here on purpose: a missing checksum is an outcome this script
# handles and reports itself, so curl's own error line only adds noise.
curl -fsL -o "$tmp/$tarball.sha256" "$base_url/$tarball.sha256" \
    || die "no checksum published for $tarball — refusing to install an unverified binary"
if ! (cd "$tmp" && sha_verify "$tarball.sha256") >/dev/null 2>&1; then
    die "checksum mismatch for $tarball — refusing to install"
fi

# --------------------------------------------------------------------------
# Install
# --------------------------------------------------------------------------

tar -xzf "$tmp/$tarball" -C "$tmp" || die "could not extract $tarball"
for component in $selected; do
    [ -f "$tmp/$component" ] || die "release $version contains no '$component' binary"
done

mkdir -p "$bin_dir" || die "could not create $bin_dir"
[ -w "$bin_dir" ] || die "$bin_dir is not writable — pick another with --bin-dir, or re-run with sudo"

for component in $selected; do
    chmod 0755 "$tmp/$component"
    # Stage inside the target dir, then rename: the rename is atomic and never
    # truncates a copy that is currently running (ETXTBSY on Linux).
    mv -f "$tmp/$component" "$bin_dir/.$component.new"
    mv -f "$bin_dir/.$component.new" "$bin_dir/$component"
    say "installed $bin_dir/$component"
done

# Run what was just installed, so an unusable binary (wrong architecture,
# too-old glibc) fails here rather than the first time it is reached for.
for component in $selected; do
    "$bin_dir/$component" --version >/dev/null 2>&1 \
        || die "$bin_dir/$component did not run here — the release may not support this platform"
done

case ":${PATH}:" in
    *":$bin_dir:"*) ;;
    *)
        say "$bin_dir is not on your PATH; add it with:"
        # shellcheck disable=SC2016  # $PATH is literal here — it's instructions to paste.
        printf '\n    export PATH="%s:$PATH"\n\n' "$bin_dir"
        ;;
esac

case "$selected" in
    *velosctl*) say "next: run 'velosctl doctor' to check your setup" ;;
esac
