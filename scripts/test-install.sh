#!/bin/sh
# End-to-end test for install.sh.
#
# Serves a fake release (a tarball of stub binaries plus its .sha256) over
# localhost and points the installer at it with VELOS_DOWNLOAD_BASE, so the
# whole path — platform detection, download, checksum verification, extraction,
# install, and the post-install smoke run — is exercised without a real release.

set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
server_pid=""
port=""

cleanup() {
    if [ -n "$server_pid" ]; then
        kill "$server_pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

# --- build a fake release -------------------------------------------------

version="v9.9.9"
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
tarball="velos-${version}-${target}.tar.gz"

assets="$work/assets/$version"
mkdir -p "$assets" "$work/stage"
for component in velosctl veloslet velos-server; do
    printf '#!/bin/sh\necho "%s %s"\n' "$component" "${version#v}" > "$work/stage/$component"
    chmod +x "$work/stage/$component"
done
tar -czf "$assets/$tarball" -C "$work/stage" velosctl veloslet velos-server
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$assets" && sha256sum "$tarball" > "$tarball.sha256")
else
    (cd "$assets" && shasum -a 256 "$tarball" > "$tarball.sha256")
fi

# --- serve it -------------------------------------------------------------

command -v python3 >/dev/null 2>&1 || fail "python3 is required to serve the fake release"

# Port 0 lets the OS pick a free port; python prints the bound one.
# -u keeps the "Serving HTTP on ... port N" line unbuffered so we can read it.
python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$work/assets" >"$work/http.log" 2>&1 &
server_pid=$!
# Up to 30s: a cold CI runner can take seconds just to start the interpreter.
attempt=0
while [ "$attempt" -lt 60 ]; do
    port="$(sed -n 's/.*127\.0\.0\.1 port \([0-9]*\).*/\1/p' "$work/http.log" | head -1)"
    if [ -n "$port" ]; then
        break
    fi
    if ! kill -0 "$server_pid" 2>/dev/null; then
        fail "test http server exited: $(cat "$work/http.log")"
    fi
    attempt=$((attempt + 1))
    sleep 0.5
done
if [ -z "$port" ]; then
    fail "test http server did not print its port in 30s: $(cat "$work/http.log")"
fi
base="http://127.0.0.1:$port"

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
pass "installs the requested components"

grep -q "not on your PATH" "$work/out.log" || fail "no PATH hint for a bin dir outside PATH"
pass "warns when the bin dir is not on PATH"

# --- a corrupted download must be rejected --------------------------------

printf 'not the real tarball' > "$assets/$tarball"
bad="$work/bin-bad"
if VELOS_DOWNLOAD_BASE="$base" sh "$root/install.sh" \
    --version "$version" --bin-dir "$bad" >"$work/bad.log" 2>&1; then
    fail "install accepted a tarball that does not match its checksum"
fi
grep -q "checksum mismatch" "$work/bad.log" || fail "wrong error for a checksum mismatch: $(cat "$work/bad.log")"
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
grep -q "no checksum published" "$work/nosum.log" || fail "wrong error for a missing checksum: $(cat "$work/nosum.log")"
pass "rejects a release with no published checksum"

# --- bad input ------------------------------------------------------------

if sh "$root/install.sh" --components nope --bin-dir "$work/bin-x" >"$work/args.log" 2>&1; then
    fail "install accepted an unknown component"
fi
grep -q "unknown component" "$work/args.log" || fail "wrong error for an unknown component"
pass "rejects an unknown component"

printf '\nall install.sh tests passed\n'
