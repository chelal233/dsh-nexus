#!/bin/bash
# Runs only in the disposable, native ARM64 build container from build.yml.
set -euo pipefail
test "$(uname -m)" = aarch64
test "$CARGO_BUILD_TARGET" = aarch64-unknown-linux-gnu
dnf install -y ca-certificates curl git
curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o /tmp/nexus-rustup.sh
sh /tmp/nexus-rustup.sh -y --profile minimal --default-toolchain 1.98.0
export PATH="$NEXUS_BUILD_NODE_BIN:$HOME/.cargo/bin:$PATH"
node desktop/scripts/prepare-agent.mjs
# A newer build image must not silently raise the package's glibc requirement.
node desktop/scripts/verify-linux-abi.mjs desktop/resources
