# Onyx Packaging & Distribution

Onyx targets Windows 10/11 x64 across three distribution formats: a portable ZIP archive, a WiX v4/v5 `.msi` installer, and a Windows SDK `.msix` package.

## Prerequisites

- Rust 1.95+ with the `x86_64-pc-windows-msvc` toolchain
- Windows 10/11 SDK / MSVC Build Tools (includes `makeappx.exe` for MSIX)
- PowerShell 5.1 or newer
- **Optional (for `.msi`)**: WiX Toolset v4+ (`dotnet tool install --global wix` or `winget install WiXToolset.WiXToolset`)

## 1. Build the Release Binary

From the repository root:

```powershell
cargo build --release --bin onyx
```

This produces `target\release\onyx.exe` (or `$env:CARGO_TARGET_DIR\release\onyx.exe` when `CARGO_TARGET_DIR` is set) using the optimized release profile (`fat` LTO, `opt-level = "s"`, `codegen-units = 1`, symbol stripping).

## 2. Create the Portable ZIP Package

Run the packaging script from the repository root:

```powershell
.\scripts\package-portable.ps1
```

Options:

| Flag | Description |
|---|---|
| `-SkipBuild` | Skip `cargo build` and package an existing release binary |
| `-OutputDir <dir>` | Output directory (default `dist`; supports relative or rooted paths) |
| `-BinaryPath <path>` | Explicit path to `onyx.exe` (defaults to `$env:CARGO_TARGET_DIR\release\onyx.exe` or `target\release\onyx.exe`) |

The script:

1. Builds `onyx.exe` in release mode (unless `-SkipBuild`).
2. Stages `onyx.exe`, `README.md`, `LICENSE`, and `assets\branding\onyx-icon.svg` (plus `onyx.ico` if present) into `<OutputDir>\onyx-portable-win64\`.
3. Creates `<OutputDir>\onyx-portable-win64.zip` with `Compress-Archive -Force`.
4. Writes the SHA-256 checksum to `<OutputDir>\onyx-portable-win64.zip.sha256` and prints it to the console.

## 3. Create the WiX MSI Installer

The WiX v4/v5 manifest lives at [`packaging/wix/onyx.wxs`](../packaging/wix/onyx.wxs) and packages `onyx.exe`, `README.md`, `LICENSE`, branding icons (`assets/branding/onyx.ico` and `onyx-icon.svg`), a Start Menu shortcut, per-user/per-machine install scope (`Scope="perUserOrMachine"`), and a stable `UpgradeCode`.

```powershell
.\scripts\package-wix.ps1 -SkipBuild
```

Options:

| Flag | Description |
|---|---|
| `-SkipBuild` | Skip `cargo build` and use an existing `onyx.exe` |
| `-OutputDir <dir>` | Output directory (default `dist`) |
| `-Version <semver>` | Package version (defaults to `[workspace.package].version` in `Cargo.toml`) |
| `-BinaryPath <path>` | Explicit path to `onyx.exe` |

Behavior:

1. Validates XML well-formedness and required `<Wix>` / `<Package>` elements in `packaging/wix/onyx.wxs`.
2. Checks `Get-Command wix`. If WiX Toolset v4+ is not installed, prints installation instructions and exits cleanly (`0`).
3. If `wix` is available, builds `<OutputDir>\onyx-<version>-win64.msi` and writes `<OutputDir>\onyx-<version>-win64.msi.sha256`.

## 4. Create the MSIX Package

The Windows 10/11 x64 MSIX manifest lives at [`packaging/msix/AppxManifest.xml`](../packaging/msix/AppxManifest.xml), referencing `onyx.exe`, `runFullTrust`, and MSIX tile assets under `Assets\`.

```powershell
.\scripts\package-msix.ps1 -SkipBuild
```

Options:

| Flag | Description |
|---|---|
| `-SkipBuild` | Skip `cargo build` and use an existing `onyx.exe` |
| `-OutputDir <dir>` | Output directory (default `dist`) |
| `-Version <semver>` | Package version (converted to 4-part `Major.Minor.Build.0` for MSIX; defaults to `Cargo.toml`) |
| `-BinaryPath <path>` | Explicit path to `onyx.exe` |

Behavior:

1. Validates `packaging/msix/AppxManifest.xml` XML well-formedness.
2. Stages `onyx.exe`, `README.md`, `LICENSE`, `assets\branding\*`, `AppxManifest.xml`, and generated PNG tile assets (`StoreLogo.png`, `Square44x44Logo.png`, `Square150x150Logo.png`, `Wide310x150Logo.png`) into `<OutputDir>\onyx-msix-stage\`.
3. Locates `makeappx.exe` on `PATH` or under `C:\Program Files (x86)\Windows Kits\10\bin\*\x64\makeappx.exe` (exiting cleanly if the Windows SDK is not installed).
4. Builds `<OutputDir>\onyx-<version>-win64.msix` and writes `<OutputDir>\onyx-<version>-win64.msix.sha256`.

## 5. Verify SHA-256 Checksums

After building or downloading any package, verify its integrity:

```powershell
# Compare against the published .sha256 file
Get-FileHash .\onyx-portable-win64.zip -Algorithm SHA256
Get-Content .\onyx-portable-win64.zip.sha256

# Or verify automatically
$expected = ((Get-Content .\onyx-portable-win64.zip.sha256) -split '\s+' | Select-Object -First 1).ToUpperInvariant()
$actual = (Get-FileHash .\onyx-portable-win64.zip -Algorithm SHA256).Hash.ToUpperInvariant()
if ($actual -eq $expected) { "Hash OK" } else { "Hash MISMATCH" }
```

## 6. CLI Flags

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

## 7. CI & GitHub Release Workflows

- **Continuous Integration** ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)): runs `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --release --bin onyx`, and verifies the built binary with `--version` and `--smoke-test`.
- **GitHub Releases** ([`.github/workflows/release.yml`](../.github/workflows/release.yml)): triggered on tag pushes (`v*`) and `workflow_dispatch`. Runs the full lint/test/release build suite on `windows-latest`, verifies `--version` and `--smoke-test`, packages the portable ZIP (`package-portable.ps1 -SkipBuild`), verifies its SHA-256 checksum, optionally builds the WiX MSI if `wix` is installed on the runner, uploads workflow artifacts, and publishes a GitHub Release via `softprops/action-gh-release@v2` on tag pushes.
