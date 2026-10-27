#!/bin/sh
# SPDX-License-Identifier: MIT
# Puts musl for mipsel-unknown-linux-musl (libc.a, the crt objects and
# libunwind.a) into a nightly toolchain's sysroot, where rustc looks for them
# when it links statically. Rust 1.71.1 was the last release to ship the target;
# its musl is built for the target as Rust defines it (MIPS32r2, soft float).
#
# usage: tools/mips-musl.sh TOOLCHAIN    (e.g. nightly-2026-10-24)
set -eu
target=mipsel-unknown-linux-musl
pkg=rust-std-1.71.1-$target
sha256=4fb95536ff01b04f57cd14eebb57043e7d420aabce4b0b5beb3133f3a9844fa4
lib=$(rustc +"$1" --print sysroot)/lib/rustlib/$target/lib
part=$pkg/rust-std-$target/lib/rustlib/$target/lib/self-contained
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -sSfL -o "$tmp/$pkg.tar.xz" "https://static.rust-lang.org/dist/$pkg.tar.xz"
echo "$sha256  $tmp/$pkg.tar.xz" | sha256sum -c --quiet
tar -C "$tmp" -xJf "$tmp/$pkg.tar.xz" "$part"
mkdir -p "$lib"
rm -rf "$lib/self-contained"
cp -R "$tmp/$part" "$lib/"
echo "$lib/self-contained"
