#!/bin/bash
# Hevy MCP Deploy Script — runs on VPS (e.g., ssh user@host "cd /opt/hevy-mcp && ./deploy.sh")
set -e

cd /opt/hevy-mcp
# GitHub is the source of truth — hard-sync to origin/master, discarding local drift.
git fetch origin
git reset --hard origin/master
docker compose up -d --build
echo "✓ Hevy MCP deployed"
