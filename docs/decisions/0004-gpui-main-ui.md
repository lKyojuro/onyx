# ADR 0004: GPUI (`gpui-kit` + `gpui-fast`) as Primary UI Framework

## Status
Accepted (supersedes the Slint UI choice in [ADR 0001](0001-tech-stack-and-workspace.md))

## Context
During Phase 7 and the early Phase 8 performance investigation, the Slint FemtoVG/OpenGL frontend exhibited a high idle memory footprint on Windows 11 (`143.98 MB` working set / `248.03 MB` private bytes in `release`, and `269.43 MB` working set / `455.86 MB` private bytes in `release-fast` during live sessions).

To evaluate a lighter DirectX 11 alternative without disturbing the 8 UI-free core crates, a prototype was built in `crates/onyx-ui-gpui` using `gpui-kit` (`0.11.0`) with `features = ["gpui-fast"]`.

## Measured Comparison (`measure-perf.ps1`, Windows 11 x64)

| Metric | Slint (`release-fast`) | GPUI (`gpui-fast`, `release-fast`) | Delta |
|---|---:|---:|---:|
| EXE size | `21.23 MB` | `24.09 MB` | `+2.86 MB` |
| Window-ready startup | `142.99 ms` | `232.88 ms` | `+89.89 ms` |
| 3s Idle WorkingSet64 | `269.43 MB` | `67.37 MB` | **`-75.0%`** |
| 3s Idle PrivateMemory64 | `455.86 MB` | `66.88 MB` | **`-85.3%`** |

Feature parity (indexed search via `onyx-search`, OLE drag-out with drag image, accessibility roles/labels, 31-command palette, Recycle Bin shell namespace view, dual-pane, preview, settings, archives) was verified in the GPUI crate.

## Decision
- Promote `crates/onyx-ui-gpui` into the root Cargo workspace as the sole primary UI crate producing the `onyx` binary (`onyx.exe`).
- Remove the legacy Slint crates (`crates/onyx-ui` and `crates/onyx-app`) and the `slint` / `slint-build` workspace dependencies.
- Keep all 8 core crates completely UI-agnostic (`onyx-core`, `onyx-config`, `onyx-cache`, `onyx-windows`, `onyx-fs`, `onyx-operations`, `onyx-search`, `onyx-preview`).

## Consequences
- Idle memory footprint drops by ~75% (working set) and ~85% (private bytes) on Windows 11.
- CI, release workflows, packaging scripts, and developers build and test a single UI stack.
- Release builds require the Windows SDK HLSL shader compiler (`fxc.exe`) at build time (preinstalled on `windows-latest` runners and standard Windows 11 SDK installations).
