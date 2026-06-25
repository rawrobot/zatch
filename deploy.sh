#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"
cargo build "$@"

# Kill only daemon processes (background, no controlling terminal),
# not interactive attach clients that hold the terminal.
for pid in $(pgrep -x ztch 2>/dev/null); do
  # daemons have fd 0 → /dev/null (no terminal)
  tty=$(readlink /proc/"$pid"/fd/0 2>/dev/null || echo "")
  if [ "$tty" = "/dev/null" ]; then
    kill -9 "$pid" 2>/dev/null || true
  fi
done
sleep 1

cp target/debug/ztch ./ztch
echo "deployed: $(md5sum ./ztch | cut -d' ' -f1)"
