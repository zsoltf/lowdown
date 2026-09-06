# Releasing Lowdown

The 0.1.0 candidate is built from `Cargo.lock` using `cargo build --locked
--release --bin lowdown`. No Python runtime is required. Codex CLI is optional
for reading logs and required for model summaries.

## Candidate checks

Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
and `cargo test --locked`. Tests must not contact a model or use real credentials.
Run `cargo audit --deny warnings` against the current RustSec database too (the
workflow installs cargo-audit 0.22.2). An audit is one check, not a guarantee of
security; investigate new advisories rather than silently adding ignores.
Build the optimized binary, run its `--version` and fixture digest, then open
watch against a real rollout. Exercise navigation, older history, wrapping,
both readers, page scrolling, resize, summary failure, and clean quit.
Test cached switching while another model batch is slow; cache reads must not
wait for model completion. Use a fake model process for this regression, not
subscription credentials. Exercise sustained appends and history loading and
record latency/resource observations; do not call a short smoke a soak test.

`cargo test --locked --test terminal -- --nocapture` opens the real binary in
a native pseudo-terminal (ConPTY on Windows), decodes its screen, and checks
startup, navigation, paged readers, wrapping, resize, older-history loading,
live appends, and terminal restoration. Synthetic sessions, isolated caches,
and forced fallback keep it offline. It fails rather than skips if raw mode
cannot start. The CI packaging steps repeat it against the extracted binary.
Set `LOWDOWN_TEST_BINARY` to an absolute extracted executable path to do that
locally. `LOWDOWN_TEST_STREAM_SECONDS=120` extends the append/navigation loop;
the default is twelve appends, and the maximum is ten minutes. Measure resources
separately; a short automated run is not proof of hours-long reliability.

Run terminal checks on native release hosts. A translated x86-64 Docker guest
on Apple Silicon can reject the terminal library's `TCGETS2` ioctl with ENOSYS
while the older `TCGETS` works. That result neither passes nor disproves native
Linux support; retain the failure and require the native-host check.

The GitHub build workflow checks macOS Apple Silicon and Intel, Linux x86-64
(Ubuntu 22.04), and Windows x86-64. It packages the tested binary, license and
operator docs, then smoke-tests the extracted archive. Archives include SHA-256
checksum files. This follows [GitHub's artifact workflow](https://docs.github.com/en/actions/tutorials/store-and-share-data).
Artifact uploads do not create a GitHub Release or publish to a package registry.

`scripts/release-files.txt` is the archive's product-document allowlist. Preserve
these paths so relative links work. Never copy the whole local `docs/` tree:
it can contain ignored, machine-local agent material. The archive root contains
the executable, LICENSE, and the allowlisted `docs/` tree.
macOS archive creation must suppress resource forks and extended attributes;
the smoke checks reject extra files, including AppleDouble (`._*`) entries.
The Unix packaging check uses Python 3's standard-library tar reader because
macOS `tar` can hide those entries when listing or extracting. Python is a
build-check dependency only, not shipped or needed to run Lowdown.

Use the CI packaging recipe locally for the same structure. Extract the final
archive into a new directory, verify every allowlisted file and checksum, and
follow [installation](../install.md) with a temporary user home. Test upgrade,
rollback and removal there without altering a developer's current installation.

## Publication

Confirm the GitHub owner/repository, branch and visibility before pushing.
Check `git remote -v` at publication time; do not infer the release destination
from a remote's name.

Private development history must not become public by accident. When preparing
a separate public repository, export only the reviewed tracked tree (for
example, `git archive` at the approved commit), inspect it for private data,
and import it as clean history only after public-destination approval. Never
push private development history directly to the public destination. Keep agent
state, learnings, credentials, real transcripts, and generated reports out.

After the candidate's CI matrix is green, check that `Cargo.toml` and the binary
report the intended version. A version tag must match `v` plus that version.
Download the workflow archives for that exact commit, verify their checksum
files, and attach them to the approved GitHub Release. Preserve the checksums.
Do not release when any target advertised on the release is still unverified.

Prebuilt archives are the initial distribution format. Homebrew, Scoop, winget,
and crates.io publishing are not configured. Keep `cargo install --locked
--path . --bin lowdown` as the source-install option. Install into a user-owned
directory already on PATH; do not place manually installed files in Homebrew's
directory. Keep the previous binary to roll back an upgrade.

Unsigned macOS and Windows binaries may trigger OS trust prompts. CI artifacts
are unsigned. The separate [macOS signing procedure](macos-signing.md) requires
the publisher's certificate, usable notary profile, and explicit upload approval.
Do not describe a signed binary as notarized until Apple accepts it. Windows
signing is not configured.

## Verification boundary

Local macOS checks and a configured CI matrix do not prove Linux or Windows
runtime behavior. Before release, obtain green hosted builds and terminal smoke
checks on those hosts. In particular, check Windows Codex CLI discovery and
execution with the installation method used by operators.

Record the exact commit and artifact hashes for every result. Existing CI on an
earlier commit is historical evidence, not final-candidate approval. Update the
[release notes](../releases/0.1.0.md) with the true platform/signing state before
publication, and retain explicit limits for anything not observed.
