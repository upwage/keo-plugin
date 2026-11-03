# Releasing Keo Companion

A release is a tag. The release workflow builds the app on a macOS and a Windows runner, signs
it, notarizes the macOS build, signs every update, and publishes it as a GitHub release on
`upwage/keo-plugin`. The installed app finds the update through a manifest on that repo.

## What a release makes

| Platform | Files                                         | Signing                                                  |
| -------- | --------------------------------------------- | -------------------------------------------------------- |
| macOS    | universal `.dmg`, `.app.tar.gz` + `.sig`      | Developer ID Application, notarized and stapled          |
| Windows  | `.msi`, NSIS `-setup.exe`, each with a `.sig` | Azure Artifact Signing, WebView2 embedded bootstrapper   |
| Both     | `latest.json`, `SHA256SUMS.txt`               | The manifest holds the updater signature of each file    |

The Windows installers carry the WebView2 bootstrapper. On a machine without WebView2 (some
Windows 10 builds), the bootstrapper downloads and installs the WebView2 runtime silently first.

## Channels

| Channel | Tag                         | App                                  | Manifest the app reads                                                                     |
| ------- | --------------------------- | ------------------------------------ | ------------------------------------------------------------------------------------------ |
| stable  | `companion-v0.2.0`          | Keo Companion, for customers         | `https://askkeo.ai/companion/latest.json`, then the `companion-stable` release's `latest.json` |
| staging | `companion-v0.2.0-staging`  | Keo Companion (staging), for the team | the team manifest on askkeo.ai, then the `companion-staging` release's copy                  |

The app tries the askkeo.ai manifest first and the GitHub copy second
(`plugins.updater.endpoints` in `src-tauri/tauri.conf.json`). Until askkeo.ai serves the
manifest, every update comes from the GitHub copy. The app version is the tag's `x.y.z`: the
Windows installer accepts only numbers, so the channel lives in the tag, not in the version.

The staging channel builds a separate app: its own bundle id, its own sign-in, its own Keo host.
Both channels can carry the same version number.

## Cut a release

1. Merge the change. Make sure the companion CI is green on the commit you tag.
2. Pick the version. It must be higher than the last release on that channel, or installed apps
   do not update.
3. Tag the commit and push the tag:

   ```sh
   git tag companion-v0.2.0-staging
   git push origin companion-v0.2.0-staging
   ```

4. Watch the **Keo Companion Release** workflow. The release stays a draft until both platforms
   are built, signed and checked; then the workflow publishes it and moves the channel manifest. It fails on the first missing secret, a signature
   that does not verify, or a manifest without all three platforms (`darwin-aarch64`,
   `darwin-x86_64`, `windows-x86_64`).
5. Install the new build on one Mac and one Windows machine. Check that an older install updates.
6. For stable, push `companion-v0.2.0` the same way.

## Dev builds (no signing)

This section is about the release workflow. For local builds and the keychain prompt, see `DEV.md`.

A pull request that changes the release workflow or the bundle config runs a dev build of the
staging app. To make one by hand, run the workflow (**Run workflow**, `dry_run` on). It builds both platforms unsigned, marks
the product name "dev build", and uploads the files as workflow artifacts kept 7 days. It reads no
secret and makes no release. A dispatch with `dry_run` off is refused: signed builds come only from
a tag.

macOS refuses to open an unsigned app from a download. To open a dev build, right-click it and
choose **Open**, or run `xattr -dr com.apple.quarantine "/Applications/Keo Companion dev build.app"`.

## Secrets

The workflow reads each secret from a GitHub Actions secret of the same name on the repository
that runs it, and keeps a master copy of each in the team's secret store. Before a signed build it
checks every name its platform needs, and fails with the list of missing ones. Nothing secret is
ever in this repository. Each build job holds only the signing secrets of its own platform.

| Name                                       | What it is                                                                   |
| ------------------------------------------ | ---------------------------------------------------------------------------- |
| `KEO_COMPANION_UPDATER_PRIVATE_KEY`        | The Tauri updater private key (the whole key file, one line)                  |
| `KEO_COMPANION_UPDATER_KEY_PASSWORD`       | Its password                                                                  |
| `KEO_COMPANION_UPDATER_PUBLIC_KEY`         | The matching public key (the `.pub` file); the build writes it into the app   |
| `KEO_COMPANION_APPLE_CERTIFICATE`          | The Developer ID Application certificate and key, `.p12`, base64              |
| `KEO_COMPANION_APPLE_CERTIFICATE_PASSWORD` | The `.p12` export password                                                    |
| `KEO_COMPANION_APPLE_SIGNING_IDENTITY`     | `Developer ID Application: <Company> (<TEAMID>)`                              |
| `KEO_COMPANION_APPLE_ID`                   | The Apple ID that notarizes                                                   |
| `KEO_COMPANION_APPLE_APP_PASSWORD`         | An app-specific password for that Apple ID                                    |
| `KEO_COMPANION_APPLE_TEAM_ID`              | The Apple developer team id                                                   |
| `KEO_COMPANION_AZURE_TENANT_ID`            | The Azure tenant of the Artifact Signing account                               |
| `KEO_COMPANION_AZURE_CLIENT_ID`            | An app registration with the Artifact Signing Certificate Profile Signer role  |
| `KEO_COMPANION_AZURE_CLIENT_SECRET`        | Its client secret                                                             |
| `KEO_COMPANION_WIN_SIGNING_ENDPOINT`       | The Artifact Signing endpoint, for example `https://eus.codesigning.azure.net` |
| `KEO_COMPANION_WIN_SIGNING_ACCOUNT`        | The Artifact Signing account name                                              |
| `KEO_COMPANION_WIN_SIGNING_PROFILE`        | The certificate profile name                                                  |
| `KEO_COMPANION_RELEASE_TOKEN`              | A fine-grained GitHub token: `upwage/keo-plugin` only, Contents read and write |

### Make the updater keypair (once)

Run this on a trusted machine, with a strong password when it asks:

```sh
npx --prefix tools/keo-plugin/companion tauri signer generate -w ~/.tauri/keo-companion.key
```

It writes `keo-companion.key` (private) and `keo-companion.key.pub` (public). Store the contents of
both files and the password under the names above, then delete the private key file from the
machine.

**Never lose or rotate this key casually.** Installed apps trust only the public key they were
built with. A new key means every installed app must be reinstalled by hand.

### Make the Apple certificate value

Export the Developer ID Application certificate with its private key from Keychain Access as a
`.p12` file, then store `base64 -i certificate.p12 | tr -d '\n'` as
`KEO_COMPANION_APPLE_CERTIFICATE`.

## Check a download

Each release carries `SHA256SUMS.txt`. On macOS, run `shasum -a 256 <file>`; on Windows, run
`Get-FileHash <file>` in PowerShell. The hash must match the line for that file.
