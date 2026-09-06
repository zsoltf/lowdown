# Installing Lowdown

Prebuilt archives are release candidates until published as an approved release.
Use the archive and SHA-256 file from the same release. No Python is required.
Codex CLI is optional for reading logs; model summaries need a compatible,
signed-in Codex CLI. See [quickstart](quickstart.md).

## Choose A Binary

| System | Archive target |
| --- | --- |
| Apple Silicon Mac | aarch64-apple-darwin |
| Intel Mac | x86_64-apple-darwin |
| Linux x86-64, glibc (Ubuntu 22.04 or newer baseline) | x86_64-unknown-linux-gnu |
| Windows x86-64 | x86_64-pc-windows-msvc |

Linux ARM, Alpine/musl, and Windows ARM binaries are not provided by this matrix.
See the release notes for actual verification and signing status.

## macOS And Linux

In the download directory, set `name` to the archive name without `.tar.gz`.
This example is for Apple Silicon:

Stop if the checksum check fails; do not extract or install that archive.

```sh
name=lowdown-0.1.0-aarch64-apple-darwin
shasum -a 256 -c "$name.tar.gz.sha256"
tar -xzf "$name.tar.gz"
mkdir -p "$HOME/.local/bin"
install -m 755 "$name/lowdown" "$HOME/.local/bin/lowdown"
export PATH="$HOME/.local/bin:$PATH"
lowdown --version
```

On Linux, `sha256sum -c "$name.tar.gz.sha256"` can replace `shasum`.
Keep the PATH export in your shell startup file if that directory is not
already on PATH. No `sudo` or manual files in Homebrew's directory are needed.
Do not bypass macOS security warnings; check the release's signing status.

## Windows

From PowerShell in the download directory:

```powershell
$name = 'lowdown-0.1.0-x86_64-pc-windows-msvc'
$expected = ((Get-Content "$name.zip.sha256" -Raw).Trim() -split '\s+')[0]
$actual = (Get-FileHash "$name.zip" -Algorithm SHA256).Hash
if ($actual -ine $expected) { throw 'Archive checksum mismatch' }
Expand-Archive "$name.zip" -DestinationPath .
$bin = Join-Path $env:LOCALAPPDATA 'Lowdown\bin'
New-Item -ItemType Directory -Force $bin | Out-Null
Copy-Item "$name/lowdown.exe" "$bin/lowdown.exe"
$env:Path = "$bin;$env:Path"
lowdown --version
```

Add `%LOCALAPPDATA%\Lowdown\bin` to your user PATH in Windows Environment
Variables for future terminals. Windows binaries are currently unsigned; do
not claim SmartScreen reputation or signing that the release does not have.

## Run, Upgrade, Or Remove

Enter the project directory and run `lowdown`. Use `h` / `l` to switch its
sessions. If discovery finds nothing, use `lowdown watch --session /path/to/log`.

To upgrade, quit Lowdown first, verify the new archive, keep the previous binary
as a rollback copy, then replace the installed binary. Run `lowdown --version`.
Check `command -v lowdown` (Unix) or `Get-Command lowdown` (PowerShell) if an
older installation still wins on PATH. Restore the saved binary to roll back.

To remove an archive install, delete only its installed `lowdown` or
`lowdown.exe` and remove the PATH entry if nothing else uses that directory.
Sessions are never deleted. To remove cached summaries as well, see the cache
locations and override in [quickstart](quickstart.md).

## Build From Source

From the source directory, with current stable Rust and Cargo installed:

```sh
cargo install --locked --path . --bin lowdown
```

This installs into Cargo's bin directory, normally `~/.cargo/bin` or
`%USERPROFILE%\.cargo\bin`. Use `cargo uninstall lowdown-rs` to remove that
source installation. Homebrew, winget, Scoop, and crates.io releases are not
configured; do not assume a similarly named package is this project.
