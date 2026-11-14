# Releasing AmberSight

How to make a release the built-in updater accepts. The updater (Settings > System >
Status & updates, or `ambersight update`) installs only packages listed in a
`SHA256SUMS` file signed with the release key; the public half of that key
is built into the program (`RELEASE_PUBKEY` in `src/update.rs`).

## The release key

- Private key: `~/.ambersight-release/release-key.pk8` on the owner's
  development computer (folder readable by the owner only).
- **Back it up offline** (e.g. an encrypted USB stick kept somewhere
  safe). Lost key: no further release can be installed by the updater
  until a new program with a new public key is installed by hand.
- **Never commit it**, copy it into the repository, or put it on a build
  server. `.gitignore` refuses `*.pk8` and `.ambersight-release/` as a
  safety net.
- Leaked key: make a new one (`ambersight release-keygen --out <new file>`),
  put the printed public key in `RELEASE_PUBKEY`, and release that program;
  say so in the release notes.

## Making a release

1. Set the version in `Cargo.toml` (e.g. `0.2.0`) and commit it. The
   updater checks that the new program reports exactly the version of the
   release tag.
   Also run `python3 scripts/third-party-notices.py --check`; if the
   dependencies changed, run it without `--check` and commit the new
   `THIRD-PARTY-NOTICES.txt` (every package ships it next to `LICENSE`).
2. Build the packages on each kind of computer with the existing scripts,
   and gather them in `dist/`:
   - Linux x86_64 and macOS: `scripts/package.sh`
   - Raspberry Pi / ARM64 Linux: `scripts/package-linux-arm64.sh`
   - Windows: `scripts\package.ps1`
   - any installers and .deb/.rpm packages, named as listed at the top of
     `scripts/release.sh`
3. Prepare and sign:

   ```sh
   scripts/release.sh 0.2.0
   ```

   This writes `dist/release-0.2.0/SHA256SUMS` over the packages found and
   signs it (`SHA256SUMS.sig`) with the key above, using the program built
   on this computer (`ambersight release-sign`). Check the list it prints.
4. Copy the release commit into the public repository's clone as one new
   commit (development history stays in the private repository), tag that
   commit and push it: `git tag v0.2.0`, `git push origin master`,
   `git push origin v0.2.0` (one tag per push).
5. Publish:

   ```sh
   scripts/release.sh 0.2.0 --publish
   ```

   This runs `gh release create v0.2.0` in the public repository
   (`RELEASE_REPO`, default `HuskerMinion/AmberSight`) with every package,
   `SHA256SUMS` and `SHA256SUMS.sig`; it refuses a private repository. Drafts and pre-releases are never offered by the
   updater; publish a normal release.

6. Update the package repository (apt and dnf):

   ```sh
   scripts/package-repo.sh 0.2.0 --commit
   ```

   (`release.sh --publish` runs this after the GitHub release exists.) It writes
   signed metadata, and the two newest `.deb` files, into the separate repository
   `~/projects/ambersight-packages` (`PACKAGES_REPO`) and commits it; it never
   pushes. Push that repository's `main` branch and GitHub Pages serves it at
   https://huskerminion.github.io/ambersight-packages/. Do this after the GitHub release
   is public: the dnf metadata links to that release's `.rpm` files. Running it again
   changes nothing. When `.git` there grows past a few hundred MB, squash its history
   (the repository holds only generated files): `git checkout --orphan new && git add -A
   && git commit -m "AmberSight <v>" && git branch -M main && git push -f origin main`.

   The signing key is separate from the release key: `~/.ambersight-release/apt-repo-key`
   (ed25519, no passphrase, mode 600; back it up offline, never commit it). A lost or
   leaked key means a new key, a new `ambersight-archive-keyring.*` and setup run again on
   every computer.

## Automatic checks

Servers look for a new release every 12 hours (`[update] check`, on by
default) and show it in Settings > System > Status & updates; an admin still
has to click Update. Releases must be published in the public repository
(`[update] repo`, built in as `HuskerMinion/AmberSight`); a private
repository answers "not found".

## Testing the updater without GitHub

`AMBERSIGHT_UPDATE_BASE_URL=http://127.0.0.1:8123` makes the program read
`<base>/latest.json` (the same JSON as GitHub's "latest release" answer,
with `browser_download_url`s pointing at the test server) instead of
GitHub. The signature is still checked with the built-in key, so this
alone cannot install anything unsigned. Only debug builds (`cargo build`,
never shipped) also accept a test public key from
`AMBERSIGHT_UPDATE_TEST_PUBKEY`, so a test release can be signed with a
throwaway key made by `ambersight release-keygen`. Never sign test
releases with the real key.
