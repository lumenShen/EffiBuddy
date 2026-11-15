#!/bin/sh
# Build the bash binary shipped with EffiBuddy.
#
# Usage: scripts/fetch-bash.sh <dest-dir> <linux|macos>
#
# Downloads the newest GNU bash tarball (the release pipeline runs this at
# build time, so every release picks up the latest upstream release) and
# builds it:
#   linux — static musl binary (independent of the distro's glibc version)
#   macos — normal build linked against the system libraries
# The destination receives `bash`, `BASH-COPYING` and `BASH-VERSION`.
#
# POSIX sh on purpose: macOS runners still start from bash 3.2.
set -eu

dest=${1:?usage: fetch-bash.sh <dest-dir> <linux|macos>}
mode=${2:?usage: fetch-bash.sh <dest-dir> <linux|macos>}

case "$mode" in
    linux | macos) ;;
    *)
        echo "unknown mode: $mode (expected linux or macos)" >&2
        exit 2
        ;;
esac

# Resolve the destination before cd'ing into the extracted build tree; CI
# passes a relative path (`runtime`) and the copy happens much later.
case "$dest" in
    /*) ;;
    *) dest="$(pwd)/$dest" ;;
esac

download() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1"
    else
        wget -qO- "$1"
    fi
}

echo "resolving the latest GNU bash release"
listing=$(download https://ftp.gnu.org/gnu/bash/)
version=$(printf '%s\n' "$listing" |
    sed -n 's/.*href="bash-\([0-9][0-9]*\.[0-9][0-9]*\(\.[0-9][0-9]*\)\{0,1\}\)\.tar\.gz".*/\1/p' |
    sort -t. -k1,1n -k2,2n -k3,3n |
    tail -n 1)
if [ -z "$version" ]; then
    echo "could not determine the latest bash version from ftp.gnu.org" >&2
    exit 1
fi
url="https://ftp.gnu.org/gnu/bash/bash-$version.tar.gz"
echo "latest bash: $version ($url)"

work=$(mktemp -d "${TMPDIR:-/tmp}/effibuddy-bash.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM

download "$url" >"$work/bash.tar.gz"
if command -v sha256sum >/dev/null 2>&1; then
    sha256=$(sha256sum "$work/bash.tar.gz" | cut -d' ' -f1)
else
    sha256=$(shasum -a 256 "$work/bash.tar.gz" | cut -d' ' -f1)
fi
tar -xzf "$work/bash.tar.gz" -C "$work"
cd "$work/bash-$version"

jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)

if [ "$mode" = linux ]; then
    : "${CC:=musl-gcc}"
    export CC
    ./configure --enable-static-link --without-bash-malloc --disable-nls \
        CFLAGS="-O2 -static" >/dev/null
else
    # NLS 只提供翻译，非交互命令用不到；关掉可避免链接宿主机上的 gettext。
    ./configure --without-bash-malloc --disable-nls CFLAGS="-O2" >/dev/null
fi

make -j"$jobs" >/dev/null

mkdir -p "$dest"
if [ "$mode" = linux ]; then
    strip bash
fi
cp bash "$dest/bash"
chmod 755 "$dest/bash"
cp COPYING "$dest/BASH-COPYING"

if [ "$mode" = linux ]; then
    if command -v file >/dev/null 2>&1 && ! file "$dest/bash" | grep -q 'statically linked'; then
        echo "error: $dest/bash is not statically linked" >&2
        exit 1
    fi
else
    # arm64 macOS requires a code signature; linking normally ad-hoc signs it,
    # but re-sign defensively in case the runner's linker did not.
    if command -v codesign >/dev/null 2>&1; then
        codesign --force --sign - "$dest/bash" >/dev/null 2>&1 || true
    fi
fi

{
    echo "GNU bash $version"
    echo "source: $url"
    echo "sha256(tarball): $sha256"
    echo "mode: $mode"
} >"$dest/BASH-VERSION"

echo "bash $version ready at $dest/bash"
