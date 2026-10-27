# tmpclean

[![CI](https://github.com/happyaspic-byte/tmpclean/actions/workflows/ci.yml/badge.svg)](https://github.com/happyaspic-byte/tmpclean/actions/workflows/ci.yml)

Fast, safe Windows temp folder cleaner written in Rust. It empties the
*contents* of your temp folders in parallel, never follows links out of them,
skips files that are in use or brand new, and reports exactly what it did.

Two executables share one deletion engine (`tmpclean-core`):

- `tmpclean.exe` - command line; suitable for Task Scheduler.
- `tmpclean-gui.exe` - small native window with live progress and Cancel.

## Supported Windows builds

Windows 10 version 1709 or later, Windows Server 2019 or later, and
Windows 11 - **x64 only** (target `x86_64-pc-windows-msvc`). There is no
32-bit, ARM64, Linux or macOS build.

## Usage

```text
tmpclean [--targets LIST] [--dry-run] [--yes] [--min-age DUR] [--json] [--io-permits N]
tmpclean-gui [same flags]
```

Examples:

```powershell
tmpclean --dry-run                    # preview what would be deleted
tmpclean                              # interactive: asks [y/N] before deleting
tmpclean --yes --json > report.json   # unattended run (e.g. Task Scheduler)
tmpclean --targets user-temp,inet-cache,windows-temp --yes   # include Windows\Temp
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--targets LIST` | `user-temp,inet-cache` | Comma-separated targets. `user-temp` = your `%TEMP%` (via `GetTempPath2W`, falling back to `GetTempPathW`); `inet-cache` = the Internet cache known folder; `windows-temp` = `%SystemRoot%\Temp` (opt-in only, usually needs an elevated prompt). An unknown name is a usage error (exit 2). |
| `--dry-run` | off | Enumerate and count what would be deleted; delete nothing. Needs no confirmation. |
| `--yes` | off | Confirm deletion without prompting. Required when stdin is not a terminal. |
| `--min-age DUR` | `24h` | Skip files and links changed more recently than `DUR` (newest of creation, last-write and change time). `0` disables the filter. Directories are never age-filtered. |
| `--json` | off | Print only the JSON report (schema v1, below) to stdout. |
| `--io-permits N` | per device | Override the number of concurrent deletions per physical device (env `TMPCLEAN_IO_PERMITS`). Default: 1 for spinning disks, 4 for SSDs, 2 if unknown. |
| `--help`, `--version` | | Standard. |

Confirmation: without `--yes` (and without `--dry-run`) the CLI prints the
targets and asks `[y/N]` when stdin is a terminal, and refuses with exit 3
otherwise, so a scheduled task never hangs on a prompt. The GUI shows a
5-second countdown you can cancel.

Only the **contents** of each target are deleted; the temp folders themselves
are kept. Junctions, symbolic links and mount points inside a target are
removed as links - their targets are never entered. Locked files are skipped
and counted, not retried.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Completed with no failures - including entries excluded by `--min-age`, entries skipped because they are in use, targets that do not exist, and every `--dry-run` whose targets resolved. |
| 1 | Completed, but at least one entry failed to delete (`failed > 0`), or a root could not be enumerated. |
| 2 | Usage error (bad flag or unknown target name). |
| 3 | Refused: a target root failed the safety guard, the target requires elevation, confirmation was required/declined, or the run was canceled. |

## JSON report (schema v1)

`--json` prints a single object:

```json
{
  "schema": 1,
  "outcome": "completed",
  "exit_code": 0,
  "dry_run": false,
  "targets": [
    { "name": "user-temp", "status": "resolved", "path": "C:\\Users\\me\\AppData\\Local\\Temp" }
  ],
  "counters": {
    "files_removed": 1200,
    "dirs_removed": 85,
    "links_removed": 2,
    "classic_pending_unconfirmed": 0,
    "excluded": 14,
    "skipped_in_use": 3,
    "failed": 0,
    "absent": 1,
    "canceled": 0,
    "logical_bytes_removed": 734003200
  },
  "errors": [
    { "path": "C:\\Users\\me\\AppData\\Local\\Temp\\locked.tmp", "code": 32, "disposition": "skipped_in_use" }
  ],
  "errors_truncated": false
}
```

- `outcome`: `completed`, `completed (not fully empty)`, `canceled`, `failed to enumerate`.
- `targets[].status`: `resolved`, `absent`, `requires-elevation`, `refused`, `unknown-target`.
- `counters`: `excluded` = skipped by `--min-age`; `skipped_in_use` = sharing/lock
  violations; `failed` = every other error; `absent` = vanished before deletion;
  `classic_pending_unconfirmed` = deleted via the non-POSIX fallback, whose
  removal the file system may finish only after the last handle closes;
  `logical_bytes_removed` = sum of file sizes (not disk space reclaimed).
- `errors[]`: `code` is the Windows error code; `disposition` is `failed`,
  `skipped_in_use` or `absent`. At most 1000 entries are listed;
  `errors_truncated` is `true` when more occurred (the counters stay exact).

## Differences from the legacy cleaner

The legacy tool (`임시폴더청소기.exe`) deleted immediately on launch. The
closest equivalent is:

```powershell
tmpclean --yes --min-age 0
```

Differences that remain on purpose: `desktop.ini` files inside temp folders
**are deleted** (the legacy tool skipped them); `Windows\Temp` is cleaned only
with `--targets windows-temp`; links are removed rather than followed;
read-only attributes are not rewritten up front.

## Privacy

tmpclean makes **no network connections** and collects no telemetry. Note
that the `--json` report and the GUI/CLI output contain **local file paths**
(including your user name inside profile paths); review a report before
sharing it.

## Safety limits

- Target roots are opened once, validated from the opened handle (drive
  roots, profile/system/program folders, network or removable volumes, and
  roots that are themselves links are refused), and never re-resolved.
- Every child is opened relative to its already-open parent without
  following reparse points, and classified from that handle.
- **Known limit - hostile reparse mutation under elevation:** when tmpclean
  runs elevated (for example `windows-temp` as an administrator or SYSTEM), a
  malicious *concurrent* process with write access to the temp folder could
  try to swap directories for links while the walk is in progress.
  Handle-relative, no-follow opens are designed to fail closed (such entries
  are deleted as links or reported as failures), but tmpclean does **not**
  claim proof of safe recursion against such an adversary. Do not run it
  elevated over folders writable by untrusted users unless you accept that
  risk.

## Build and release checks

```powershell
cargo build --release --workspace
pwsh scripts/check-release.ps1 target/release   # PE32+ x64, ASLR/HEVA/NX/CFG, no UPX, size budget
pwsh scripts/test-check-release.ps1             # proves the checker fails on tampered/malformed exes
```

Release binaries are x64, built with Control Flow Guard, restrict DLL loading
to System32 at startup, embed an `asInvoker` + `longPathAware` + Common
Controls v6 manifest, and are **never UPX-packed** (size budget: CLI <= 1.5
MiB, GUI <= 2 MiB). They are ready for Authenticode signing; see
`scripts/sign.ps1` (documentation only, not run in CI) for `signtool` with a
PFX or Azure Trusted Signing.

## License

MIT
