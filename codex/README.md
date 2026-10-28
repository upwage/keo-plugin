# Keo for Codex

Keo is your AI teammate in Slack. With this plugin, Keo can see your Codex sessions, send them work
from Slack, and carry your Allow or Deny back to a Codex permission prompt.

## What you need

- The Codex CLI, version 0.160 or later (`codex --version`).
- A Slack workspace that has Keo.

## Install (once)

1. Add the marketplace and install the plugin:

   ```sh
   codex plugin marketplace add upwage/keo-plugin
   codex plugin add keo@keo-plugin
   ```

2. Sign in with Slack. Your browser opens, and you pick your workspace:

   ```sh
   codex mcp login keo
   ```

   If you already connected Keo to Codex (an `[mcp_servers.keo]` entry in your Codex config),
   skip this step. A server you configured yourself wins over the plugin's server of the same
   name, so the plugin uses the connection you already have, wherever that entry points.

3. Trust the plugin's hooks. Codex runs no plugin hook until you review it. Start Codex, type
   `/hooks`, and trust the four Keo hooks (SessionStart, UserPromptSubmit, Stop and
   PermissionRequest). Each one calls Keo's `coding_session_event` tool on the `keo` server you just
   signed in to. None of them runs a command on your machine.

That's it. Start Codex as usual. In Slack, ask Keo "what are my Codex sessions doing?"

## What Keo can do

- **See your sessions.** Each session shows its folder, its state (working, waiting for permission,
  finished, idle or stale), what you last asked, and Codex's last answer.
- **Send a session work.** Tell Keo in Slack, for example "tell the api session to add a test".
  Keo first shows you the exact message on an approval card, and nothing is sent until you press
  Allow; each message is approved on its own. Keo hands Codex your message when the session's
  current turn ends, and posts Codex's answer back to the Slack thread. Keo tells you "Delivered"
  only once Codex has started on it.
- **Answer a permission prompt.** In a turn you started from Slack, Keo posts Codex's permission
  request to the thread with Allow and Deny. If no one answers in time, it is denied. In a turn you
  started at the keyboard, Codex asks you in the terminal as usual.

Only you can see or drive your sessions. Your teammates, and Keo's own automations, can't.

## Good to know

- **After a plugin update, trust the hooks again.** Codex trusts a hook by its exact definition.
  If an update changes a hook, Codex skips that hook until you trust it again in `/hooks`. Until
  then, Keo stops hearing from your sessions.
- **An idle session waits for its next turn.** Codex cannot wake an idle session from a plugin, so
  a message sent to a session that is not working reaches it when you next start a turn there.
- **A closed session shows as stale, not ended.** Codex does not let a plugin report a session's end
  to Keo, so a session you closed goes stale after two hours of silence.
- **`codex exec` runs never ask for permission,** so Keo has no prompt to answer there. Keo still
  sees those sessions and can send them work.

## Remove it

Sign out first, while Codex still knows the plugin's server, then remove the plugin:

```sh
codex mcp logout keo
codex plugin remove keo@keo-plugin
```
