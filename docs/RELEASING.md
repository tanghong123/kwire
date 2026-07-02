# Releasing Kwire

A release ships two Homebrew artifacts from the shared tap `tanghong123/tap`:

- **cask `kwire`** — the desktop app, installed from the release asset
  `Kwire_universal.app.tar.gz` (a **signed + notarized** universal `Kwire.app`).
- **formula `kwire-cli`** — the TUI, built from source at the tag.

## Flow

1. **Bump the version** in `Cargo.toml`, `app/src-tauri/Cargo.toml`, and
   `app/src-tauri/tauri.conf.json`; refresh `Cargo.lock`; commit; push `main`.

2. **CI drafts the release** (`.github/workflows/release.yml`): on a version
   bump it builds the universal `.app` **unsigned** + the universal TUI binary
   into a **draft** GitHub release `v<version>`. (Immutable releases mean a
   *published* release can't accept new assets, so the build targets a draft.)

3. **Sign locally and replace the .app asset.** CI cannot sign — the Developer
   ID certificate and the `notarytool` keychain profile live in the maintainer's
   login keychain, not in CI. Run:

   ```bash
   scripts/sign-macos-app.sh v<version>
   ```

   which builds the universal `Kwire.app`, Developer-ID signs it (hardened
   runtime), notarizes it via `xcrun notarytool … --keychain-profile ytdl-notarize`,
   staples the ticket, packages `dist/Kwire_universal.app.tar.gz`, and uploads it
   to the draft with `--clobber`. Override `SIGN_IDENTITY` / `NOTARY_PROFILE` if
   your keychain differs.

4. **(Optional) attach the tour video**, then **publish**:

   ```bash
   gh release upload v<version> demo/out/kwire-tour.mp4   # optional
   gh release edit   v<version> --draft=false             # publish → creates the tag
   ```

5. **Homebrew auto-bumps.** Publishing fires `.github/workflows/homebrew-bump.yml`,
   which recomputes the cask `sha256` from the published (signed) `.app.tar.gz`
   and the formula source tarball, and pushes the bump to `tanghong123/homebrew-tap`.
   Requires the repo secret `HOMEBREW_TAP_TOKEN`.

## Prerequisites for signing

- An Apple Developer account and a **Developer ID Application** certificate in
  the login keychain (`security find-identity -p codesigning -v` lists it).
- A `notarytool` keychain profile created once:

  ```bash
  xcrun notarytool store-credentials ytdl-notarize \
      --apple-id "<apple-id>" --team-id "<TEAM_ID>" --password "<app-specific-password>"
  ```
