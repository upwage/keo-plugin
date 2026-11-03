#!/usr/bin/env node
// Runs `tauri dev` with the given arguments (`npm run dev`, `npm run dev:staging`).
//
// On macOS it first picks a code-signing identity and makes Cargo run each dev build through
// scripts/dev-sign-run.sh, which signs the binary before it starts. Every rebuild then carries the
// same signature, so the keychain asks for its password once and not on every build (DEV.md).
//
// The identity is, in order: APPLE_SIGNING_IDENTITY, `bundle.macOS.signingIdentity` in
// src-tauri/tauri.conf.json (overridden by any `--config` file or JSON given to this script), the first "Developer ID Application" identity in the keychain, then
// the first "Apple Development" one. With none, or with APPLE_SIGNING_IDENTITY set to "-", the
// build keeps the linker's ad-hoc signature, and the keychain asks again after every build.
import { execFileSync, spawn } from 'node:child_process';
import { chmodSync, copyFileSync, mkdtempSync, readFileSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

/** The `bundle.macOS.signingIdentity` Tauri would see: the base config, then each `--config` in order. */
function configuredIdentity(args) {
  const read = (v) => {
    try {
      return JSON.parse(v.trim().startsWith('{') ? v : readFileSync(resolve(ROOT, v), 'utf8'));
    } catch {
      return null;
    }
  };
  let identity = read(join(ROOT, 'src-tauri/tauri.conf.json'))?.bundle?.macOS?.signingIdentity;
  for (let i = 0; i < args.length; i++) {
    const m = /^(?:-c|--config)(?:=(.*))?$/.exec(args[i]);
    if (!m) continue;
    const value = m[1] ?? args[++i];
    const found = value && read(value)?.bundle?.macOS?.signingIdentity;
    if (found !== undefined && found !== null) identity = found;
  }
  return identity;
}

/**
 * The identity to sign dev builds with, as `{ sign, name }`, or null for the ad-hoc signature. An
 * identity found in the keychain is signed by its SHA-1: two certificates can share a name (after a
 * renewal, or one Apple Development certificate per Mac), and `codesign` refuses an ambiguous name.
 */
function signingIdentity() {
  const fromEnv = process.env.APPLE_SIGNING_IDENTITY;
  if (fromEnv) return fromEnv === '-' ? null : { sign: fromEnv, name: fromEnv };
  const fromConfig = configuredIdentity(process.argv.slice(2));
  if (fromConfig && fromConfig !== '-') return { sign: fromConfig, name: fromConfig };
  let listed = '';
  try {
    listed = execFileSync('security', ['find-identity', '-v', '-p', 'codesigning'], {
      encoding: 'utf8',
    });
  } catch {
    return null;
  }
  const found = [...listed.matchAll(/^\s*\d+\)\s+([0-9A-F]{40})\s+"([^"]+)"/gm)].map((m) => ({
    sign: m[1],
    name: m[2],
  }));
  return (
    found.find((i) => i.name.startsWith('Developer ID Application:')) ??
    found.find((i) => i.name.startsWith('Apple Development:')) ??
    null
  );
}

/** A copy of dev-sign-run.sh at a path with no whitespace, or null. */
function runnerCopy() {
  const dir = realpathSync(mkdtempSync(join(tmpdir(), 'keo-dev-sign-')));
  if (/\s/.test(dir)) return null;
  const copy = join(dir, 'dev-sign-run.sh');
  copyFileSync(join(ROOT, 'scripts/dev-sign-run.sh'), copy);
  chmodSync(copy, 0o755);
  return copy;
}

const env = { ...process.env };
if (process.platform === 'darwin') {
  const identity = signingIdentity();
  // Cargo splits a runner from the environment on whitespace, so the runner path must have none:
  // a checkout path can, so run a copy from a fresh temporary folder.
  const runner = identity && runnerCopy();
  if (identity && !runner) {
    console.log('The temporary folder path has a space: dev builds stay unsigned (DEV.md).');
  } else if (identity) {
    env.KEO_DEV_SIGNING_IDENTITY = identity.sign;
    // CARGO_TARGET_<TRIPLE>_RUNNER for both Mac targets.
    for (const triple of ['AARCH64_APPLE_DARWIN', 'X86_64_APPLE_DARWIN'])
      env[`CARGO_TARGET_${triple}_RUNNER`] = runner;
    console.log(`Signing dev builds with "${identity.name}" (DEV.md).`);
  } else {
    console.log(
      'No code-signing identity found: dev builds keep an ad-hoc signature, so the keychain asks ' +
        'for its password after every rebuild (DEV.md).',
    );
  }
}

// The CLI's own entry point through Node, with no shell, so every argument keeps its boundaries
// (a Windows shell would split a path with a space).
const cli = join(ROOT, 'node_modules/@tauri-apps/cli/tauri.js');
const child = spawn(process.execPath, [cli, 'dev', ...process.argv.slice(2)], {
  cwd: ROOT,
  env,
  stdio: 'inherit',
});
child.on('exit', (code, signal) => process.exit(signal ? 1 : (code ?? 1)));
