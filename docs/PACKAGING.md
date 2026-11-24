# Onyx Packaging & Distribution

Onyx targets Windows 11 x64. The current distribution format is a portable ZIP package; installer-based distribution (MSIX / WiX) is planned (see [ROADMAP.md](ROADMAP.md)).

## Prerequisites

- Rust 1.95+ with the `x86_64-pc-windows-msvc` toolchain
- Windows 11 SDK / MSVC Build Tools
- PowerShell 5.1 or newer

## 1. Build the Release Binary

From the repository root:

```powershell
cargo build --release --bin onyx
```

This produces `target\release\onyx.exe` using the optimized release profile (thin LTO, symbol stripping).

## 2. Create the Portable ZIP Package

Run the packaging script from the repository root:

```powershell
.\scripts\package-portable.ps1
```

Options:

| Flag | Description |
|---|---|
| `-SkipBuild` | Skip `cargo build` and package the existing `target\release\onyx.exe` |
| `-OutputDir <dir>` | Output directory (default `dist`) |

The script:

1. Builds `onyx.exe` in release mode (unless `-SkipBuild`).
2. Stages `onyx.exe`, `README.md`, `LICENSE`, and `assets\branding\onyx-icon.svg` (plus `onyx.ico` if present) into `<OutputDir>\onyx-portable-win64\`.
3. Creates `<OutputDir>\onyx-portable-win64.zip` with `Compress-Archive -Force`.
4. Writes the SHA-256 checksum to `<OutputDir>\onyx-portable-win64.zip.sha256` and prints it to the console.

## 3. Verify the SHA-256 Hash

After downloading or distributing the package, verify its integrity:

```powershell
# Compare against the published .sha256 file
Get-FileHash .\onyx-portable-win64.zip -Algorithm SHA256
Get-Content .\onyx-portable-win64.zip.sha256

# Or verify automatically
$expected = (Get-Content .\onyx-portable-win64.zip.sha256) -split '\s+' | Select-Object -First 1
$actual = (Get-FileHash .\onyx-portable-win64.zip -Algorithm SHA256).Hash
if ($actual -eq $expected) { "Hash OK" } else { "Hash MISMATCH" }
```

## 4. CLI Flags

The packaged `onyx.exe` supports the following command-line flags:

| Flag | Description |
|---|---|
| `--version` | Print the Onyx version and exit |
| `--smoke-test` | Run a startup smoke test (initialize the app, then exit) |
| `[PATH]` | Open Onyx at the given directory path |

Examples:

```powershell
.\onyx.exe --version
.\onyx.exe --smoke-test
.\onyx.exe D:\Projects
```

## 5. Roadmap: Installer Distribution

The portable ZIP is the Phase 6 baseline. Planned distribution channels:

- **MSIX**: Microsoft Store-ready packaging with identity, clean install/uninstall, and automatic updates.
- **WiX (MSI)**: Classic Windows Installer with per-machine/per-user options, upgrade codes, and Start Menu shortcuts.

Both will be produced from the same release binary and staged by `scripts/package-portable.ps1` (or a sibling script) so checksums stay reproducible.
