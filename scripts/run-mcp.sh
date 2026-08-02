#!/usr/bin/env bash
#
# run-mcp.sh — launch the local mail-mcp binary for MCP stdio clients
#
# Why this exists
# ---------------
# The server calls dotenvy::dotenv(), which loads `.env` from the process
# current working directory. Many MCP hosts spawn tools with a CWD that is
# not the repo root, so `.env` is never found and accounts fail to load.
# This wrapper always cds to the repository root before exec.
#
# What it does
# ------------
# 1. Resolve the repo root from this script's location
# 2. Require a release binary at target/release/mail-mcp
# 3. Require a .env file at the repo root (copy from .env.example)
# 4. exec the binary, forwarding all arguments (stdio stays attached)
#
# Usage
# -----
#   cargo build --release
#   cp -n .env.example .env   # then fill credentials
#   ./scripts/run-mcp.sh
#
# Point your MCP client command at this script (absolute path recommended).
# Prefer injecting MAIL_* env vars in the client config when you do not want
# a local .env file.
#
# Not required for production installs (npx, Docker, GitHub release binaries).
# Those pass configuration through the environment, not a repo-local .env.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN="$ROOT/target/release/mail-mcp"
if [[ ! -x "$BIN" ]]; then
  echo "mail-mcp: missing release binary at $BIN" >&2
  echo "Build it with: cargo build --release" >&2
  exit 1
fi

if [[ ! -f "$ROOT/.env" ]]; then
  echo "mail-mcp: missing $ROOT/.env (copy from .env.example and fill credentials)" >&2
  exit 1
fi

exec "$BIN" "$@"
