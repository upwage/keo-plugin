# Keo for Claude Code and Codex

Keo is your AI teammate in Slack. With this plugin, Keo can see your Claude Code or Codex sessions,
and you can steer them from Slack:

- **See what your sessions are doing.** Ask Keo "what are my coding sessions doing?" For each
  session it shows the folder, whether it is working, waiting for your permission or finished, what
  you last asked, and the assistant's last reply.
- **Send a session work.** Tell Keo what a session should do next. The session gets your message
  when it next finishes a turn, and its reply comes back to the same Slack thread.
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
sign in with Slack in the browser window that opens. Until you sign in, the plugin sends nothing.

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
