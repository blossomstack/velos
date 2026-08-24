#!/bin/sh
# End-to-end test for install.sh.
#
# Builds a fake release (a tarball of stub binaries plus its .sha256) in a temp
# directory and points the installer at it with VELOS_DOWNLOAD_BASE, so the whole
# path — platform detection, fetch, checksum verification, extraction, install,
# and the post-install smoke run — is exercised without a real release.
#
# The base is a `file://` URL rather than a local HTTP server: curl treats both
# the same, and standing up a server first made this test depend on an
# interpreter starting promptly on a cold CI runner, which it did not.

set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"

trap 'rm -rf "$work"' EXIT INT TERM

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

# Write the checksum file the installer insists on, in the format it verifies.
write_sha() { # $1 = directory, $2 = file within it
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$1" && sha256sum "$2" > "$2.sha256")
    else
        (cd "$1" && shasum -a 256 "$2" > "$2.sha256")
    fi
}

# --- build a fake release -------------------------------------------------

case "$(uname -s)" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-gnu" ;;
    *) fail "unsupported test platform: $(uname -s)" ;;
esac
case "$(uname -m)" in
    arm64|aarch64) arch_part="aarch64" ;;
    x86_64|amd64) arch_part="x86_64" ;;
    *) fail "unsupported test architecture: $(uname -m)" ;;
esac
target="${arch_part}-${os_part}"

version="v9.9.9"
tarball="velos-${version}-${target}.tar.gz"
assets="$work/assets/$version"
mkdir -p "$assets" "$work/stage"
for component in velosctl veloslet velos-server; do
    printf '#!/bin/sh\necho "%s %s"\n' "$component" "${version#v}" > "$work/stage/$component"
    chmod +x "$work/stage/$component"
done
tar -czf "$assets/$tarball" -C "$work/stage" velosctl veloslet velos-server
write_sha "$assets" "$tarball"

base="file://$work/assets"

# --- the happy path -------------------------------------------------------

bin="$work/bin"
VELOS_DOWNLOAD_BASE="$base" sh "$root/install.sh" \
    --version "$version" --bin-dir "$bin" --components velosctl,veloslet >"$work/out.log" 2>&1 \
    || fail "install failed: $(cat "$work/out.log")"

[ -x "$bin/velosctl" ] || fail "velosctl was not installed"
[ -x "$bin/veloslet" ] || fail "veloslet was not installed"
if [ -e "$bin/velos-server" ]; then
    fail "velos-server was installed but not requested"
fi
if ls "$bin"/.*.new >/dev/null 2>&1; then
    fail "a staging file was left behind in $bin"
fi
[ "$("$bin/velosctl")" = "velosctl 9.9.9" ] || fail "the installed velosctl is not the released one"
pass "installs the requested components"

grep -q "not on your PATH" "$work/out.log" || fail "no PATH hint for a bin dir outside PATH"
pass "warns when the bin dir is not on PATH"

# --- the default bin dir --------------------------------------------------

# HOME is redirected so the default path is exercised without touching the real
# one — the default is a user-visible promise, not just an internal constant.
home="$work/home"
mkdir -p "$home"
VELOS_DOWNLOAD_BASE="$base" HOME="$home" sh "$root/install.sh" \
    --version "$version" >"$work/default.log" 2>&1 \
    || fail "install with the default bin dir failed: $(cat "$work/default.log")"
[ -x "$home/.local/bin/velosctl" ] || fail "the default bin dir is not ~/.local/bin"
pass "installs into ~/.local/bin by default"

# The default component set is a user-visible promise too: a machine that runs
# `install.sh` with no flags must end up able to both drive the control plane
# (velosctl) and join it as a worker (veloslet) -- but must not silently gain a
# control plane of its own (velos-server).
[ -x "$home/.local/bin/veloslet" ] || fail "veloslet is not installed by default"
if [ -e "$home/.local/bin/velos-server" ]; then
    fail "velos-server was installed by default but is not in the default set"
fi
pass "installs velosctl and veloslet by default"

# --- a binary that cannot run here must be rejected ------------------------

broken="v9.9.8"
broken_tarball="velos-${broken}-${target}.tar.gz"
broken_assets="$work/assets/$broken"
mkdir -p "$broken_assets" "$work/broken-stage"
printf '#!/bin/sh\nexit 3\n' > "$work/broken-stage/velosctl"
chmod +x "$work/broken-stage/velosctl"
tar -czf "$broken_assets/$broken_tarball" -C "$work/broken-stage" velosctl
write_sha "$broken_assets" "$broken_tarball"

# Pinned to velosctl: this case is about a binary that will not execute, and
# the fake release deliberately ships only that one component.
if VELOS_DOWNLOAD_BASE="$base" sh "$root/install.sh" \
    --version "$broken" --bin-dir "$work/bin-broken" --components velosctl \
    >"$work/broken.log" 2>&1; then
    fail "install accepted a binary that does not run on this machine"
fi
grep -q "did not run here" "$work/broken.log" \
    || fail "wrong error for an unrunnable binary: $(cat "$work/broken.log")"
pass "rejects a binary that will not run on this machine"

# --- a corrupted download must be rejected --------------------------------

printf 'not the real tarball' > "$assets/$tarball"
bad="$work/bin-bad"
if VELOS_DOWNLOAD_BASE="$base" sh "$root/install.sh" \
    --version "$version" --bin-dir "$bad" >"$work/bad.log" 2>&1; then
    fail "install accepted a tarball that does not match its checksum"
fi
grep -q "checksum mismatch" "$work/bad.log" \
    || fail "wrong error for a checksum mismatch: $(cat "$work/bad.log")"
if [ -e "$bad/velosctl" ]; then
    fail "a binary was installed despite the checksum mismatch"
fi
pass "rejects a tarball that does not match its checksum"

# --- a missing checksum must be rejected ----------------------------------

rm -f "$assets/$tarball.sha256"
if VELOS_DOWNLOAD_BASE="$base" sh "$root/install.sh" \
    --version "$version" --bin-dir "$work/bin-nosum" >"$work/nosum.log" 2>&1; then
    fail "install accepted an unverifiable release"
fi
grep -q "no checksum published" "$work/nosum.log" \
    || fail "wrong error for a missing checksum: $(cat "$work/nosum.log")"
pass "rejects a release with no published checksum"

# --- an empty checksum file must be rejected ------------------------------

# GNU sha256sum reports success for a file containing no checksum lines, so this
# is the case where verification could silently pass on nothing.
: > "$assets/$tarball.sha256"
if VELOS_DOWNLOAD_BASE="$base" sh "$root/install.sh" \
    --version "$version" --bin-dir "$work/bin-emptysum" >"$work/emptysum.log" 2>&1; then
    fail "install accepted an empty checksum file"
fi
grep -q "empty or names another file" "$work/emptysum.log" \
    || fail "wrong error for an empty checksum: $(cat "$work/emptysum.log")"
pass "rejects an empty checksum file"

# --- bad input ------------------------------------------------------------

if sh "$root/install.sh" --components nope --bin-dir "$work/bin-x" >"$work/args.log" 2>&1; then
    fail "install accepted an unknown component"
fi
grep -q "unknown component" "$work/args.log" || fail "wrong error for an unknown component"
pass "rejects an unknown component"

printf '\nall install.sh tests passed\n'
