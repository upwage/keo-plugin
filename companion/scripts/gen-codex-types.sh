#!/bin/sh
# Generates the JSON Schema of the Codex app-server protocol from the codex binary on this
# machine, into a temporary folder (or the folder given as the first argument).
#
# This script only produces the schema. The Codex runner work turns that schema into Rust types
# in src-tauri/src/runner/codex_types.rs; this script does not write that file.
set -eu

if ! command -v codex >/dev/null 2>&1; then
  echo "codex is not on PATH; install Codex first." >&2
  exit 1
fi

out=${1:-$(mktemp -d "${TMPDIR:-/tmp}/codex-schema.XXXXXX")}
mkdir -p "$out"
codex app-server generate-json-schema --out "$out"
echo "Codex version: $(codex --version)"
echo "Schema written to: $out"
