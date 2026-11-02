# Developing Keo Companion

## Run a dev build

```sh
npm install
npm run dev            # the customer app
npm run dev:staging    # the team's staging app (its own bundle id, sign-in and Keo host)
```

On macOS, if the Xcode license is not accepted, prefix the command with
`DEVELOPER_DIR=/Library/Developer/CommandLineTools`.

## Sign dev builds so the keychain asks once (macOS)

The app keeps its sign-in in the login keychain, under a service named after the bundle id. The
keychain trusts the app by its code signature. `tauri dev` does not sign: each build gets a new
ad-hoc signature from the linker, so after every rebuild macOS asks for the login keychain password
again.

`npm run dev` and `npm run dev:staging` fix this. `scripts/dev.mjs` picks a code-signing identity,
and Cargo runs each build through a copy of `scripts/dev-sign-run.sh` in a temporary folder
(Cargo cannot run a path with a space), which signs the binary with that
identity before it starts. The signature then names the same certificate on every build, so the
keychain keeps its trust. Choose **Always Allow** at the first prompt, and it does not come back.

The identity is the first one found, in this order:

1. The `APPLE_SIGNING_IDENTITY` environment variable.
2. `bundle.macOS.signingIdentity` in `src-tauri/tauri.conf.json` (empty in this repository; do not
   commit a personal identity).
3. The first `Developer ID Application` identity in your keychain.
4. The first `Apple Development` identity in your keychain. It works as well as a Developer ID for
   dev builds.

To see what you have, and to pick one:

```sh
security find-identity -v -p codesigning
APPLE_SIGNING_IDENTITY="Developer ID Application: <Company> (<TEAMID>)" npm run dev:staging
```

The first signed run can ask once more: `codesign` itself asks to use the identity's private key.
Choose **Always Allow** there too.

To check a build, run `codesign -d -r- src-tauri/target/debug/keo-companion`. A signed build shows
`certificate leaf[subject.CN] = "<your identity>"`. An ad-hoc build shows `cdhash H"..."`, which
changes on every build.

### No identity on the machine

With no identity, or with `APPLE_SIGNING_IDENTITY=-`, the build keeps the linker's ad-hoc signature.
**The keychain then still asks for the password after every rebuild.** `npm run dev` says so when it
starts. To stop the prompts without a paid Apple account, sign in to Xcode with any Apple ID
(Settings > Accounts > Manage Certificates > Apple Development), which makes an `Apple Development`
identity that step 4 picks up.

A release build is signed in CI and does not use any of this (`RELEASE.md`).
