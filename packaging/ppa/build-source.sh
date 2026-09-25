#!/bin/sh
# Build signed Ubuntu source packages for the Launchpad PPA.
#
#   packaging/ppa/build-source.sh [SERIES...]      (default: noble)
#
# Environment:
#   PPA_REV=N        packaging revision within the PPA (default 1); bump it
#                    to re-upload the same upstream version
#   DEBSIGN_KEYID=K  GPG key to sign with (default: debsign's own choice)
#   UNSIGNED=1       skip signing, for a local dry run
#   PPA=owner/name   upload target shown at the end
#                    (default gyuyoung-kim/icecream-watcher)
#
# Output goes to target/ppa/. The orig tarball there is reused if present:
# Launchpad refuses a second, different tarball under the same name, so
# delete it only when the upstream version changes.
set -eu

top=$(git rev-parse --show-toplevel)
cd "$top"

pkg=icecream-watcher
ver=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
rev=${PPA_REV:-1}
out=$top/target/ppa
orig=$out/${pkg}_$ver.orig.tar.xz
[ $# -gt 0 ] || set -- noble

mkdir -p "$out"

if [ ! -e "$orig" ]; then
    command -v cargo-vendor-filterer >/dev/null || {
        echo "needs cargo-vendor-filterer: cargo install cargo-vendor-filterer" >&2
        exit 1
    }
    stage=$(mktemp -d)
    trap 'rm -rf "$stage"' EXIT
    git archive --prefix="$pkg-$ver/" HEAD -- . ':!debian' ':!docs/demo.gif' |
        tar -x -C "$stage"
    # Only crates a Linux build reaches; the Windows ones are most of the
    # weight and are replaced by stubs.
    (cd "$stage/$pkg-$ver" &&
        cargo vendor-filterer --platform='*-unknown-linux-gnu' vendor >/dev/null)
    mtime=$(git log -1 --format=%ct HEAD)
    tar -C "$stage" --sort=name --mtime="@$mtime" --owner=0 --group=0 \
        --numeric-owner -cJf "$orig" "$pkg-$ver"
    rm -rf "$stage"
    trap - EXIT
fi

sign=
[ -z "${UNSIGNED:-}" ] || sign="-us -uc"
[ -z "${DEBSIGN_KEYID:-}" ] || sign="-k$DEBSIGN_KEYID"

for series in "$@"; do
    src=$out/$pkg-$ver
    rm -rf "$src"
    tar -C "$out" -xJf "$orig"
    cp -a debian "$src/"
    # The committed changelog says 0.1.0-1 UNRELEASED; each series gets
    # 0.1.0-1~<series><rev>, which sorts below any official Ubuntu package.
    sed -i "1s/^$pkg ($ver-1) UNRELEASED;/$pkg ($ver-1~${series}$rev) $series;/" \
        "$src/debian/changelog"
    head -1 "$src/debian/changelog" | grep -q "~${series}$rev) $series;" || {
        echo "debian/changelog does not start with $pkg ($ver-1) UNRELEASED" >&2
        exit 1
    }
    # -d, -nc: the build dependencies need only exist on Launchpad.
    (cd "$src" &&
        debuild -S -sa -d -nc $sign)
    rm -rf "$src"
done

echo
echo "Upload with:"
for series in "$@"; do
    echo "  dput ppa:${PPA:-gyuyoung-kim/icecream-watcher} $out/${pkg}_$ver-1~${series}${rev}_source.changes"
done
