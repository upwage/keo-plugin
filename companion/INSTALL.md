# Install Keo Companion

<!-- Draft of the install page on askkeo.ai. The release workflow fills SHA256SUMS.txt on each
     release; copy the lines for the current version into "Check your download". -->

Keo Companion is a small menu-bar app. It lets Keo start and message Claude Code and Codex
sessions on your computer, in folders you approve, when you approve a task in Slack. It has no
main window. It is open source (MIT): the code is in the `companion/` folder of
`github.com/upwage/keo-plugin`.

## Requirements

- macOS 12 or newer (Apple silicon or Intel), or Windows 10 or 11 (64-bit).
- Claude Code 2.1.303 or newer, or Codex 0.158.0 or newer. Each Keo Companion release also names
  the newest Codex it was tested with; a newer Codex waits for the next companion update.
- A Keo account in your Slack workspace.

## Download

| Platform | File                                  |
| -------- | ------------------------------------- |
| macOS    | `Keo Companion_<version>_universal.dmg` |
| Windows  | `Keo Companion_<version>_x64-setup.exe` (or the `.msi` for managed installs) |

Download from the latest release at `github.com/upwage/keo-plugin/releases`.

## Install

### macOS

1. Open the `.dmg` file.
2. Drag **Keo Companion** to **Applications**.
3. Open **Keo Companion** from Applications. A Keo icon appears in the menu bar.
4. Click the icon, then click **Sign in with Keo**.
5. Finish the sign-in in your browser.
6. Under **Approved folders**, add the folders Keo may work in.

### Windows

1. Run the `-setup.exe` file. If WebView2 is missing, the installer adds it first (it carries the WebView2 bootstrapper).
2. Open **Keo Companion** from the Start menu. A Keo icon appears in the notification area.
3. Click the icon, then click **Sign in with Keo**.
4. Finish the sign-in in your browser.
5. Under **Approved folders**, add the folders Keo may work in.

The app checks for an update at launch. Every release is signed with the Keo release key, and the
app accepts an update only with a valid signature from that key. This early release reports an
update in its log and does not install it yet: to update, install the new version from the
releases page.

## What leaves your computer

- **Where:** askkeo.ai only, and the update check (askkeo.ai, then the release manifest on
  github.com/upwage/keo-plugin).
- **What:** session ids, the task text you approved in Slack, tool events, usage numbers, and,
  for a session Keo started or sent work to, its progress notes, its questions and its last
  reply. Those can quote code or files from that session.
- **Never:** full transcripts, files read off disk, passwords, or your Claude or OpenAI tokens.
  Sessions Keo did not hand work to are never sent.
- **Sign-in:** OAuth in your browser. The app never sees your password.

## What stays on your computer

- The app's settings: a machine id, the approved folders and your switches.
  macOS: `~/Library/Application Support/ai.askkeo.companion/`.
  Windows: `%APPDATA%` under `ai.askkeo.companion`.
- The Keo sign-in token, in the macOS keychain or the Windows Credential Manager, under
  `ai.askkeo.companion`.
- A log, one file a day, kept 7 days. It never holds task text, prompts or tokens.

## Check your download

Each release has a `SHA256SUMS.txt` file. Compare it with the hash of your file:

- macOS: `shasum -a 256 "Keo Companion_<version>_universal.dmg"`
- Windows (PowerShell): `Get-FileHash ".\Keo Companion_<version>_x64-setup.exe"`

| File | SHA-256 |
| ---- | ------- |
| (filled from `SHA256SUMS.txt` for each release) | |

You can also check the signature. On macOS, run
`codesign --verify --deep --strict "/Applications/Keo Companion.app"`. On Windows, right-click the
file, choose **Properties**, then **Digital Signatures**.

## Pause Keo

Click the menu-bar icon and turn on **Pause**. While paused, Keo cannot start or message a session
on this computer. The choice is kept across restarts.

## Uninstall

1. Click the menu-bar icon, then click **Sign out**. This revokes the app's access at Keo,
   deletes the stored token and the settings, and turns off start at login.
2. Quit the app (**Quit** in the panel).
3. Remove the app:
   - macOS: drag **Keo Companion** from Applications to the Trash.
   - Windows: **Settings**, **Apps**, **Installed apps**, **Keo Companion**, **Uninstall**.
4. To be sure nothing is left:
   - macOS: delete `~/Library/Application Support/ai.askkeo.companion`, and the
     `ai.askkeo.companion` item in Keychain Access.
   - Windows: delete the `ai.askkeo.companion` folder under `%APPDATA%`, and the
     `ai.askkeo.companion` entry in Credential Manager.
