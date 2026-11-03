#!/bin/sh
# Cargo runner for macOS dev builds, set by scripts/dev.mjs: signs the freshly built binary with
# KEO_DEV_SIGNING_IDENTITY, then runs it with its arguments. A stable signature keeps the
# keychain's trust in the app across rebuilds (DEV.md).
set -eu
codesign --force --sign "$KEO_DEV_SIGNING_IDENTITY" "$1"
exec "$@"
