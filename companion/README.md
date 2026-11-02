# Keo Companion

A small menu-bar app for macOS and Windows. It connects Keo to the coding agents on this machine (Codex and
Claude Code). When you approve a task for this machine in Slack, Keo rings the companion, and the
companion starts or messages the coding session in an approved folder. It has no main window and
no Dock icon: click the menu-bar icon to open the panel.

The menu-bar icon shows the state: signed out, listening, paused, working, needs attention, or
needs update (a coding tool is outside the versions this build drives: older than the floor, or
newer than the newest version tested, in which case the companion itself needs an update).

Status: scaffold. Sign-in, the doorbell, the inbox and the registry are in place. The Codex runner
drives its own `codex app-server` (0.158.0 or newer) on a loopback websocket: it starts sessions,
sends, steers, stops, and relays approvals and questions. The Claude Code runner only detects the
installed version for now.

## Install (development build)

You need Rust (stable), Node 22, and the Xcode command line tools.

1. Open a terminal in `tools/keo-plugin/companion`.
2. Run `npm ci`.
3. Run `npm run dev`. The first build takes a few minutes.
4. Click the new icon in the menu bar, then click **Sign in with Keo**.
5. Finish the sign-in in your browser, then come back to the panel.
6. Add the folders Keo may work in under **Approved folders**.

To build an app bundle, run `npm run tauri build`. A local build is not signed; signed builds come
from the release workflow (`RELEASE.md`). To install a release, see `INSTALL.md`.

If the build stops with "You have not agreed to the Xcode license agreements", either accept the
license (`sudo xcodebuild -license`) or build with the command line tools:
`DEVELOPER_DIR=/Library/Developer/CommandLineTools npm run dev`.

## What leaves the machine

- Hosts: askkeo.ai only, plus the update check (askkeo.ai, then the release manifest on
  github.com/upwage/keo-plugin).
- What it sends: session ids, the task text you approved in Slack, tool events, usage numbers, and, for a session Keo started or sent work to, its progress notes, its questions and its last reply. Those can quote code or files from that session.
- No full transcripts, no files read off disk, no vendor tokens. Sessions Keo did not hand work to
  are never relayed.

In detail:

- The sign-in uses OAuth with PKCE in your browser. The companion never sees your password.
- The doorbell sends only a random machine id. It holds no sign-in and carries no task text.
- Every other call goes to Keo's MCP server with your sign-in.
- Text sent to Keo is cut to the server's limits before it leaves the machine.
- The update check reads the update manifest from askkeo.ai, then from the public release on
  github.com/upwage/keo-plugin.

## What stays on the machine

- `~/Library/Application Support/ai.askkeo.companion/state.json`: the machine id, the OAuth
  client id, the approved folders, and the switches (pause, start at login).
- The macOS keychain, service `ai.askkeo.companion`, entry `keo-refresh-token`: the refresh
  token. The access token is kept in memory only.
- `~/Library/Application Support/ai.askkeo.companion/log/`: one log file a day, kept 7 days.
  The log never holds task text, prompts or tokens.

## How to revoke access

1. Open the panel and click **Sign out**. The companion revokes its token at Keo, deletes the
   keychain entry and `state.json`, and turns off start at login.
2. To remove the app as well, quit it and delete it from Applications.
3. To be sure nothing is left, delete the keychain item `ai.askkeo.companion` in Keychain Access
   and the folder `~/Library/Application Support/ai.askkeo.companion`.

## Pause

The **Pause** switch stops the doorbell. While paused, Keo cannot start or message a session on
this machine. Your choice is kept across restarts.

## Layout

- `index.html`, `src/main.ts`, `src/styles.css`: the panel (plain TypeScript, no framework).
- `src-tauri/src/`: the app.
  - `state.rs`: the state machine and `state.json`.
  - `auth.rs`: sign-in, tokens, keychain.
  - `keo.rs`: Keo's MCP client and the event contract.
  - `doorbell.rs`: the long poll.
  - `inbox.rs`: pull and ack tasks.
  - `relay.rs`: runner events to Keo, tasks to runners.
  - `registry.rs`: the session roster.
  - `dirs.rs`: approved folders.
  - `log.rs`: the daily log.
  - `runner/`: one file per coding tool, behind the `Runner` trait. `runner/codex/` holds the Codex
    runner's websocket client, app-server launcher, frame mapping and its fake-server tests.
- `scripts/gen-icons.mjs`: draws the tray and app icons.
- `scripts/gen-codex-types.sh`: writes the Codex app-server JSON Schema for the Codex runner.

## Configuration

`src-tauri/tauri.conf.json` sets `plugins.keo.baseUrl`. The app accepts only `https://askkeo.ai`
or one of its subdomains, and falls back to `https://askkeo.ai` for anything else. The bundle
identifier is the keychain service name, so a second build with its own identifier keeps its own
sign-in. `npm run dev:staging` merges `../staging/companion/tauri.conf.json` over the default
config. On macOS both dev scripts sign each build, so the keychain asks for its password once
(`DEV.md`).

The updater public key in `tauri.conf.json` is a placeholder: the release build writes the real
key in, so a local build's update check only logs its result. The version pins for Claude Code and
Codex live in `src-tauri/src/versions.rs`.

## License

MIT. See `../LICENSE`.
