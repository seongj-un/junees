#!/bin/sh
# Installs the junees binary from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/seongj-un/junees/main/install.sh | sh
#
# Settings (environment variables):
#   JUNEES_INSTALL_DIR  folder to install into (default: ~/.local/bin)
#   JUNEES_VERSION      release to install, such as v0.1.0 (default: the latest)
#
# It needs no sudo: everything goes into JUNEES_INSTALL_DIR. The archive is
# checked against the release's SHA256SUMS before anything is installed, and
# the binary is put in place with one rename.
set -eu

repo_releases="https://github.com/seongj-un/junees/releases"

say() {
    printf 'junees install: %s\n' "$*" >&2
}

fail() {
    say "error: $*"
    exit 1
}

# JUNEES_DOWNLOAD_BASE replaces the release address in tests; only a server
# on this computer is taken.
base="${JUNEES_DOWNLOAD_BASE:-$repo_releases}"
base="${base%/}"
case "$base" in
    "$repo_releases") ;;
    http://127.0.0.1:* | http://127.0.0.1/* | http://localhost:* | http://localhost/* | http://\[::1\]:* | http://\[::1\]/*) ;;
    *) fail "JUNEES_DOWNLOAD_BASE must be a server on this computer (it is only for tests)" ;;
esac

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux) os_part="unknown-linux-musl" ;;
        Darwin)
            os_part="apple-darwin"
            # A shell under Rosetta says x86_64 on an Apple silicon Mac.
            if [ "$arch" = "x86_64" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = "1" ]; then
                arch="arm64"
            fi
            ;;
        *) fail "there is no junees release for $os; build it from source: cargo install --git https://github.com/seongj-un/junees" ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch_part="x86_64" ;;
        aarch64 | arm64) arch_part="aarch64" ;;
        *) fail "there is no junees release for $os on $arch; build it from source: cargo install --git https://github.com/seongj-un/junees" ;;
    esac
    printf '%s-%s\n' "$arch_part" "$os_part"
}

if command -v curl >/dev/null 2>&1; then
    fetcher=curl
elif command -v wget >/dev/null 2>&1; then
    fetcher=wget
else
    fail "neither curl nor wget is installed"
fi

# download <url> <file>
download() {
    if [ "$fetcher" = curl ]; then
        if [ "$base" = "$repo_releases" ]; then
            curl --proto '=https' --tlsv1.2 -fsSL -o "$2" "$1"
        else
            curl -fsSL -o "$2" "$1"
        fi
    else
        # Only options BusyBox wget (Alpine, small containers) knows too. GNU
        # wget's --https-only would not help: it applies to links followed in
        # recursive mode, not to redirects. The archive is checked against
        # SHA256SUMS either way.
        wget -q -O "$2" "$1"
    fi
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | sed 's/^.*= *//'
    else
        fail "no sha256sum, shasum or openssl to check the download with"
    fi
}

target=$(detect_target)
install_dir="${JUNEES_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/junees-install.XXXXXX")
cleanup() {
    rm -rf "$tmp"
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

version="${JUNEES_VERSION:-}"
version="${version#v}"
if [ -n "$version" ]; then
    case "$version" in
        *[!0-9A-Za-z.-]*) fail "JUNEES_VERSION is not a version: $version" ;;
    esac
    say "downloading SHA256SUMS of v$version"
    download "$base/download/v$version/SHA256SUMS" "$tmp/SHA256SUMS" ||
        fail "download failed ($fetcher): $base/download/v$version/SHA256SUMS (is v$version a release?)"
else
    say "looking for the latest release"
    download "$base/latest/download/SHA256SUMS" "$tmp/SHA256SUMS" ||
        fail "download failed ($fetcher): $base/latest/download/SHA256SUMS (is a release published?)"
    # The archive names carry the version: junees-<version>-<target>.tar.gz.
    version=$(sed -n "s/^[0-9a-fA-F]\{64\}  *\*\{0,1\}junees-\([0-9A-Za-z.-]*\)-$target\.tar\.gz$/\1/p" "$tmp/SHA256SUMS" | head -n 1)
    [ -n "$version" ] || fail "the latest release has no build for $target"
fi

archive="junees-$version-$target.tar.gz"
expected=$(awk -v name="$archive" '$2 == name || $2 == "*" name { print $1; exit }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "SHA256SUMS of v$version lists no $archive"

say "downloading $archive"
download "$base/download/v$version/$archive" "$tmp/$archive" ||
    fail "download failed ($fetcher): $base/download/v$version/$archive"

actual=$(sha256_of "$tmp/$archive")
expected=$(printf '%s' "$expected" | tr 'A-F' 'a-f')
if [ "$actual" != "$expected" ]; then
    fail "$archive does not match SHA256SUMS (got $actual, expected $expected); nothing was installed"
fi

mkdir "$tmp/unpacked"
tar -xzf "$tmp/$archive" -C "$tmp/unpacked" "junees-$version-$target/junees" ||
    fail "cannot unpack $archive"
binary="$tmp/unpacked/junees-$version-$target/junees"
if [ ! -f "$binary" ] || [ -L "$binary" ]; then
    fail "$archive holds no junees binary"
fi

mkdir -p "$install_dir" || fail "cannot create $install_dir"
staged="$install_dir/.junees.install.$$"
cp "$binary" "$staged" || fail "cannot write in $install_dir (set JUNEES_INSTALL_DIR to a folder of yours)"
chmod 755 "$staged"
if ! mv -f "$staged" "$install_dir/junees"; then
    rm -f "$staged"
    fail "cannot replace $install_dir/junees"
fi

say "installed junees $version ($target) as $install_dir/junees"
case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *)
        say "$install_dir is not on your PATH; add it, for example in ~/.profile:"
        say "  export PATH=\"$install_dir:\$PATH\""
        ;;
esac
