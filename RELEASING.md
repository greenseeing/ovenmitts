# Releasing ovenmitts

Releases are built and published by Woodpecker CI on Codeberg. Pushing a `v*`
tag triggers `.woodpecker.yml`, which cross-compiles static musl binaries
(amd64 + arm64) and publishes them — with SHA-256 sums and `install.sh` — to a
Codeberg release. `install.sh` then resolves the latest release and downloads
the binary for the host architecture.

## One-time CI setup

Do this **before** pushing your first tag. A tag pipeline that runs without the
secret builds fine and then fails at the publish step, leaving a tag with no
release behind it.

1. Enable the repo in Woodpecker: <https://ci.codeberg.org/repos/add> → select
   `greenseer/ovenmitts`.
2. Generate a Codeberg access token: **Settings → Applications → Manage Access
   Tokens → Generate Token**, scope **`write:repository`** (copy it now — it is
   shown once).
3. Add it as a Woodpecker repo secret named **`codeberg_token`** (Repository →
   **Settings → Secrets**). Repo secrets are automatically available to `tag`
   pipelines — no per-event configuration needed.

Codeberg's shared runners are amd64-only; the arm64 binary is cross-compiled
with `cargo zigbuild`. Per Codeberg's shared-runner request, the build caps
cargo at `-j 4`.

## Cutting a release

1. Update `CHANGELOG.md`: add a `## [x.y.z] - YYYY-MM-DD` section and the
   matching link reference at the bottom.

2. Bump `version` in `Cargo.toml`, then sync `Cargo.lock`. The CI build passes
   `--locked`, so the lockfile must already match the manifest:

   ```bash
   cargo check            # rewrites Cargo.lock's ovenmitts entry
   cargo check --locked   # must pass — this is what CI does
   ```

   Run the sync with a **plain** `cargo check`. Passing `--locked` to the first
   command cannot work: it refuses to update a stale lockfile and exits 101.

3. Verify green. The test suite drives the pipeline against the fake tool
   stubs in `tests/fakebin/` — no burner, disc, or system packages needed:

   ```bash
   cargo test
   cargo clippy --all-targets -- -D warnings
   ```

4. Optionally reproduce the CI build locally before tagging (needs `zig` and
   `cargo install cargo-zigbuild --locked`) — see the manual release below.

5. Commit, tag, and push. The tag must point at the commit that carries the
   final `.woodpecker.yml`; Woodpecker reads the config from the tagged commit,
   not from `main`.

   ```bash
   git commit -am "Release x.y.z"
   git tag -a vx.y.z -m "ovenmitts x.y.z"
   git push origin main         # push event: no pipeline (when: only matches tags)
   git push origin vx.y.z       # tag event: builds and publishes the release
   ```

   The tag pipeline builds the binaries and **creates the release with its
   assets** — do not create the release or upload files by hand.

If the publish step fails, delete the remote tag and any partial release, fix
the cause, and re-tag:

```bash
git push origin :refs/tags/vx.y.z
git tag -d vx.y.z
```

## Manual release (CI unavailable)

Reproduce the CI build locally, then create the release and upload the assets
yourself.

```bash
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo install cargo-zigbuild --locked   # needs zig on PATH — https://ziglang.org/download

cargo zigbuild --release --locked -j 4 \
  --target x86_64-unknown-linux-musl \
  --target aarch64-unknown-linux-musl --bin ovenmitts

mkdir -p dist
cp target/x86_64-unknown-linux-musl/release/ovenmitts dist/ovenmitts-linux-amd64
cp target/aarch64-unknown-linux-musl/release/ovenmitts dist/ovenmitts-linux-arm64
cp install.sh dist/install.sh
( cd dist && for f in ovenmitts-linux-amd64 ovenmitts-linux-arm64; do sha256sum "$f" > "$f.sha256"; done )
```

Then create a release for the tag on Codeberg (**Releases → New release →**
select `vx.y.z`) and upload every file in `dist/`: the two binaries, their
`.sha256` files, and `install.sh`. Asset names must stay exactly
`ovenmitts-linux-<arch>` and `ovenmitts-linux-<arch>.sha256` — `install.sh`
resolves them by that name, and refuses to install a binary whose `.sha256`
sidecar is missing.

## Verify

On a target device:

```bash
curl -fsSL https://codeberg.org/greenseer/ovenmitts/raw/branch/main/install.sh | bash
ovenmitts --version   # prints the new version
```
