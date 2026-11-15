#!/bin/sh
# Sync this checkout to a Linux development host and run a command there.
#   KIRISAME_DEV_HOST=user@host scripts/remote.sh cargo test
# The checkout lands in ~/kirisame on the remote host.
set -eu
host=${KIRISAME_DEV_HOST:?set KIRISAME_DEV_HOST to the Linux host to build on}
here=$(cd "$(dirname "$0")/.." && pwd)
rsync -a --delete --exclude target --exclude .git "$here/" "$host:kirisame/"
ssh "$host" "cd kirisame && . \$HOME/.cargo/env && $*"
