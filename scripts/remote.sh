#!/bin/sh
# Sync this checkout to a Linux development host and run a command there.
#   GANTRY_DEV_HOST=user@host scripts/remote.sh cargo test
# The checkout lands in ~/gantry on the remote host.
set -eu
host=${GANTRY_DEV_HOST:?set GANTRY_DEV_HOST to the Linux host to build on}
here=$(cd "$(dirname "$0")/.." && pwd)
rsync -a --delete --exclude target --exclude .git "$here/" "$host:gantry/"
ssh "$host" "cd gantry && . \$HOME/.cargo/env && $*"
