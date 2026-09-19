#!/bin/sh
# Builds the ad-hoc signed .app and .dmg into src-tauri/target/release/bundle/.
set -eu
cd "$(dirname "$0")/.."

# Rust embeds absolute source paths (panic locations) in the binary. Remap the
# builder's home directory so a release does not carry their username.
export RUSTFLAGS="--remap-path-prefix=$HOME=/build ${RUSTFLAGS:-}"

# Skips the DMG's Finder window styling, which needs GUI automation permission
# and hangs without it. The DMG still has the app and the Applications link.
export CI=true

pnpm install --frozen-lockfile
pnpm tauri build --bundles app,dmg
