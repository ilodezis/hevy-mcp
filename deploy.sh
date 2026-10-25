#!/bin/bash
# Hevy MCP deploy script — runs on the VPS: ssh <host> "/path/to/hevy-mcp/deploy.sh"
set -e

# Operate on the checkout this script lives in, wherever that happens to be —
# a hard-coded path only works on the one machine it was written for.
cd "$(dirname "$0")"
# GitHub is the source of truth — hard-sync to origin/master, discarding local drift.
git fetch origin
git reset --hard origin/master
docker compose up -d --build
echo "✓ Hevy MCP deployed"
