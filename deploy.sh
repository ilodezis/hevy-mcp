#!/bin/bash
# Hevy MCP deploy script — runs on the VPS: ssh <host> "/path/to/hevy-mcp/deploy.sh"
set -euo pipefail

# Operate on the checkout this script lives in, wherever that happens to be —
# a hard-coded path only works on the one machine it was written for.
cd "$(dirname "$0")"
# GitHub is the source of truth — hard-sync to origin/master, discarding local drift.
git fetch origin
PREV=$(git rev-parse HEAD)
git reset --hard origin/master
docker compose up -d --build

healthy=0
for _ in $(seq 1 60); do
  if curl -fsS "http://127.0.0.1:8010/" >/dev/null 2>&1; then
    healthy=1
    break
  fi
  sleep 1
done

if [ "$healthy" -ne 1 ]; then
  echo "ROLLBACK: hevy-mcp unhealthy after deploy — rolling back to $PREV" >&2
  git reset --hard "$PREV"
  docker compose up -d --build
  echo "ROLLBACK COMPLETE: restored $PREV" >&2
  exit 1
fi

echo "✓ Hevy MCP deployed"
