#!/bin/bash
# Runs only in the disposable, matching native build container from build.yml.
set -euo pipefail
case "$CARGO_BUILD_TARGET" in
  x86_64-unknown-linux-gnu) machine=x86_64 ;;
  aarch64-unknown-linux-gnu) machine=aarch64 ;;
  *) echo "Unsupported Linux target: $CARGO_BUILD_TARGET" >&2; exit 1 ;;
esac
test "$(uname -m)" = "$machine"
dnf install -y ca-certificates curl git perl-core make
curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o /tmp/nexus-rustup.sh
sh /tmp/nexus-rustup.sh -y --profile minimal --default-toolchain 1.98.0
export PATH="$NEXUS_BUILD_NODE_BIN:$HOME/.cargo/bin:$PATH"
node desktop/scripts/prepare-agent.mjs
# A newer build image must not silently raise the package's glibc requirement.
node desktop/scripts/verify-linux-abi.mjs desktop/resources
