#!/usr/bin/env bash
# Materialise vendor/rhai: pristine rhai plus rhaigrain's visibility patch.
#
# rhaigrain is built on rhai's AST and evaluation plumbing, and needs more of it
# public than even rhai's `internals` feature exposes. It carries the delta as a
# patch and wires it up with `[patch.crates-io]` — which Cargo only honours from
# the *top-level* workspace, so it does nothing when rhaigrain is a dependency.
# This repo therefore has to apply the same patch itself, and everything here
# builds against the result: firmware, validator-wasm, script-env.
#
# The patch is fetched from rhaigrain at the pinned revision rather than copied
# in. One source of truth: a second copy here would be a second thing to keep in
# step with the rev in crates/script-env/Cargo.toml.
#
# Run after a clone, and any time the pinned revision changes.
#
#   scripts/vendor-rhai.sh

set -euo pipefail

# Must match `rhai` in Cargo.toml and the `rev` on rhaigrain in
# crates/script-env/Cargo.toml.
RHAI_VERSION="1.25.1"
RHAI_SHA256="dd4dd0f8c36625202a4ba553c416c19b719947cd2a31d1bda06126e4a5727daf"
RHAIGRAIN_REV="c9d6ab156b671cf22f28f4cad417100f1ba15ee7"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
vendor_dir="$repo_root/vendor"
target_dir="$vendor_dir/rhai"

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

# sha256sum is coreutils', shasum is Perl's; macOS ships only the latter.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

tarball="$work_dir/rhai.crate"
# Prefer the local registry cache so a rebuild needs no network; the checksum is
# verified either way, so a poisoned cache is caught rather than trusted.
cached=$(find "${CARGO_HOME:-$HOME/.cargo}/registry/cache" -name "rhai-$RHAI_VERSION.crate" 2>/dev/null | head -1)
if [ -n "$cached" ]; then
  cp "$cached" "$tarball"
else
  curl -fsSL "https://static.crates.io/crates/rhai/rhai-$RHAI_VERSION.crate" -o "$tarball"
fi

actual=$(sha256_of "$tarball")
if [ "$actual" != "$RHAI_SHA256" ]; then
  echo "rhai-$RHAI_VERSION.crate checksum mismatch" >&2
  echo "  expected $RHAI_SHA256" >&2
  echo "  got      $actual" >&2
  exit 1
fi

tar -xzf "$tarball" -C "$work_dir"
pristine="$work_dir/rhai-$RHAI_VERSION"

patch_file="$work_dir/rhai.patch"
curl -fsSL \
  "https://raw.githubusercontent.com/ImTheSquid/rhaigrain/$RHAIGRAIN_REV/vendor/rhai-$RHAI_VERSION.patch" \
  -o "$patch_file"

# An empty patch is a valid state upstream, and `git apply` errors on one.
if grep -q '^@@' "$patch_file"; then
  git -C "$pristine" apply --whitespace=nowarn "$patch_file"
  echo "applied $(grep -c '^@@' "$patch_file") hunk(s)"
else
  echo "patch has no hunks; vendoring pristine $RHAI_VERSION"
fi

rm -rf "$target_dir"
mkdir -p "$vendor_dir"
mv "$pristine" "$target_dir"

echo "vendored rhai $RHAI_VERSION -> ${target_dir#"$repo_root"/}"
