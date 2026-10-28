# Keo for Claude Code and Codex

Keo is your AI teammate in Slack. With this plugin, Keo can see your Claude Code or Codex sessions,
and you can steer them from Slack:

- **See what your sessions are doing.** Ask Keo "what are my coding sessions doing?" For each
  session it shows the folder, whether it is working, waiting for your permission or finished, what
  you last asked, and the assistant's last reply.
- **Send a session work.** Tell Keo what a session should do next. Keo first shows you the exact
  message on an approval card in Slack, and nothing is sent until you press Allow. Each message is
  approved on its own. A session that is working gets the message when its current turn ends. An
  idle Claude Code session is woken to take it at once; an idle Codex session gets it at the end of
  its next turn. The session's reply comes back to the same Slack thread.
- **Answer permission prompts.** During a turn that a message from Slack started, when the session
  asks to run a command or edit a file, Keo shows you the request in that Slack thread with Allow
  and Deny buttons. If nobody answers in time, the request is denied. During a turn you started at
  the keyboard, prompts appear in your terminal as usual.

You need a Slack workspace where Keo is installed. The plugin installs for your user account, so
every session you start reports to Keo.

## Install for Claude Code

Requires Claude Code 2.1.303 or later.

```sh
claude plugin marketplace add upwage/keo-plugin
claude plugin install keo@keo-plugin
```

Then sign in once. In Claude Code, run `/mcp`, choose `plugin:keo:keo`, then **Authenticate**, and
sign in with Slack in the browser window that opens. Until you sign in, the only thing the plugin
sends is the wake check's session id (see below), and Keo knows nothing to answer it with.

## Install for Codex

```sh
codex plugin marketplace add upwage/keo-plugin
codex plugin add keo@keo-plugin
```

Then sign in with Slack and trust the plugin's hooks once. The Codex guide,
[codex/README.md](codex/README.md), has every step, including what to do after an update and when
you already have a `keo` server in Codex.

## What Keo receives

The plugin reports these moments in each session:

| When                                 | What is sent                                                                       |
| ------------------------------------ | ---------------------------------------------------------------------------------- |
| You send a prompt                    | The prompt text                                                                    |
| The assistant finishes a turn        | Its final message for that turn, and whether a hook already asked it to keep going |
| The assistant asks for permission    | The tool name, and the command or file path (in Codex, the tool's full input)      |
| Claude Code shows you a notification | The notification's text and kind (Claude Code only)                                |
| A session starts or ends             | How it started (Codex only), or why it ended (Claude Code only)                    |

Each report also carries the session's id, the folder it runs in, and (from Codex) which app sent
it. Keo does not read your files or your code. It sees only what is listed here, which can include
code if your prompts, the assistant's replies or a tool's input contain some.

Everything goes over your own signed-in connection to Keo, so Keo knows each report is yours.

## The one command it runs (Claude Code)

The reports above are calls Claude Code makes to Keo's tools; none of them runs a command on your
machine. The plugin adds one command, for Claude Code only. After each turn ends, Claude Code runs it
in the background for up to a day (it stops sooner when the session's next turn starts):

- It sends this session's id, and nothing else, to `https://askkeo.ai/api/mcp/doorbell` with
  `curl`, and asks whether a message you approved in Slack is waiting. No token or other secret is
  sent, and no message text comes back: Keo answers only "ring" or "wait". Each check also tells
  Keo that the session can be woken, so before you approve a message Keo can say whether it will
  wake the session or wait for the next turn you start.
- When Keo answers "ring", the command ends and Claude Code wakes the idle session with a short note
  that a message from you is waiting. The session ends that turn, and the plugin's usual end-of-turn
  report then hands it your message, over your signed-in connection.
- It keeps one small file in your temp folder, named by a checksum of the session id, so that a newer
  check replaces the older one instead of running beside it. The id is never in a file name or in
  the command's arguments.

Keo answers "ring" only for a message you approved, and only while the session is between turns,
at most three times per message, two minutes apart. The check needs `curl` and a POSIX shell: macOS, Linux, and Windows with Git Bash
(which Claude Code uses there). Without them it does nothing, and a message waits for the end of the
session's next turn.

## Only you can drive your sessions

Your sessions belong to the Slack account you signed in with.

- Only you can see them, send them work, or answer their permission prompts. Other people in your
  workspace, guests and bots cannot, and neither can Keo's own scheduled tasks.
- A message you send from Slack reaches the session as a message from you, and the assistant treats
  it as if you had typed it in the session.
- If you ask about your sessions in a channel, Keo's answer is visible to the people in that
  channel, like any Keo reply. Ask in a direct message to keep it to yourself.

## Keo without the plugin

You can also connect Keo as an MCP server only, without this plugin. Keo then cannot see or drive
your sessions. For Claude Code:

```sh
claude mcp add --transport http keo https://askkeo.ai/api/mcp
```

## Turn it off or remove it

Claude Code:

- Pause it: `claude plugin disable keo@keo-plugin`, and `claude plugin enable keo@keo-plugin` to turn
  it back on.
- Sign out: run `/mcp`, choose `plugin:keo:keo`, then clear its authentication.
- Remove it:

  ```sh
  claude plugin uninstall keo@keo-plugin
  claude plugin marketplace remove keo-plugin
  ```

Codex: see [codex/README.md](codex/README.md#remove-it).

## License

MIT. See [LICENSE](LICENSE).
