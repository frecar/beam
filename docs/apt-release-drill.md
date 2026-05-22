# Beam APT Release Drill

Beam hosts its Debian package repository from the `gh-pages` branch and serves it through `raw.githubusercontent.com`. The release drill proves that a tag reaches GitHub Actions, produces signed APT metadata, and becomes visible to `apt update`.

## Scope

This runbook covers the public Beam release path:

- Git tag to GitHub Actions release workflow.
- `.deb` artifacts for `amd64` and `arm64`.
- `gh-pages` APT metadata refresh.
- GPG signature verification.
- `apt update` visibility for the latest version.

Monitoring and production host orchestration are out of scope for this repo — they belong in your own deployment infrastructure. This runbook only covers the public release pipeline that produces the signed APT artifacts you then consume.

## Public Verifier

Run the verifier against the published APT repository:

```bash
scripts/verify-apt-repo.sh 0.3.23
```

The script checks:

- `InRelease` and detached `Release.gpg` signatures against `gpg/beam.gpg`.
- `Release` SHA256 entries for both architecture `Packages` files.
- `Packages` metadata version for `amd64` and `arm64`.
- Published `.deb` SHA256 checksums for both architectures.
- Native `apt-get update` candidate visibility when `apt-get` is available.

Use `BEAM_APT_REPO_URL` to test another repository root with the same layout:

```bash
BEAM_APT_REPO_URL=https://raw.githubusercontent.com/frecar/beam/gh-pages scripts/verify-apt-repo.sh 0.3.23
```

## Drill Procedure

1. Create a normal PR that bumps `Cargo.toml`, `Cargo.lock`, `web/package.json`, and `web/package-lock.json` to the patch version.
2. Run `make version-check` and the relevant CI checks before merging.
3. Merge the PR to `main`.
4. From a clean, up-to-date `main`, run `make release VERSION=X.Y.Z`.
5. Watch the `Release` workflow for tag `vX.Y.Z` until all jobs pass.
6. Confirm GitHub Releases has `beam_X.Y.Z_amd64.deb`, `beam_X.Y.Z_arm64.deb`, tarballs, and `checksums.txt`.
7. Run `scripts/verify-apt-repo.sh X.Y.Z`.
8. Record the observed timing in the GitHub issue or release checklist.

## GPG Rotation

The release workflow imports the private key from the `APT_GPG_PRIVATE_KEY` GitHub secret and publishes the matching public key to `gpg/beam.gpg`.

To rotate the key:

1. Generate a new signing key dedicated to the Beam APT repository.
2. Replace `APT_GPG_PRIVATE_KEY` in GitHub Actions secrets.
3. Run a patch release.
4. Verify `scripts/verify-apt-repo.sh X.Y.Z` succeeds and prints the new fingerprint.
5. Keep old and new key material available to operators during the transition window so hosts can update their trusted keyring before the old key is retired.
