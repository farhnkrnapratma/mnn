#!/usr/bin/env bash

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0

for hook in pre-commit commit-msg; do
  src="$repo_root/.githooks/$hook"
  dest="$repo_root/.git/hooks/$hook"
  [ -f "$src" ] || continue
  cp "$src" "$dest"
  chmod +x "$dest"
done
