# Context Capsule CLI

Context Capsule captures the **working state around a project** and restores it semantically: Git context, developer tools, terminals, VS Code, Zen/Firefox, Docker resources, desktop applications/windows, Explorer folders, and display placement where supported.

The CLI is local-first. Capsule data is stored in SQLite on the machine; browser and editor adapters synchronize their semantic state through local runtime files/native messaging rather than a hosted service.

## Core workflow

```powershell
# Inspect what Context Capsule currently sees
cargo run -- inspect --verbose

# Save the first revision
cargo run -- save work

# Capture the current state as a new immutable revision
cargo run -- update work

# See retained revisions
cargo run -- history work

# Compare two points in time
cargo run -- diff work@1 work@2

# Preview an older revision without changing the machine
cargo run -- restore work@1 --dry-run

# Restore it
cargo run -- restore work@1

# Diagnose the local installation/adapters
cargo run -- doctor --verbose
```

After installing the binary, replace `cargo run --` with `capsule`.

## Immutable revisions

A capsule name points to its latest state, while each update remains addressable as `name@revision`.

```text
work        latest revision
work@1      original captured state
work@2      second captured state
```

Both of these create a new revision when `work` already exists:

```powershell
capsule update work
capsule save work --force
```

Existing databases are migrated in place. A capsule created before revision support is retained as revision `1`; Context Capsule cannot reconstruct versions that were overwritten by older builds before revision history existed.

Deleting a capsule deletes the capsule and all of its revisions. Individual historical revisions are intentionally immutable.

## Semantic diff

`capsule diff` compares meaning rather than raw JSON ordering.

```powershell
capsule diff work@2 work@5
capsule diff work@2 work@5 --json
```

Current diff sections include workspace/system context, Git, Zen/Firefox tabs and named groups, VS Code workspace/tabs/integrated terminals, external terminal sessions, Docker resources, desktop applications, and developer tool versions.

Duplicate browser/editor tabs are treated as a multiset, so adding or removing one copy is represented once instead of collapsing duplicates accidentally.

## Doctor

```powershell
capsule doctor
capsule doctor --verbose
capsule doctor --json
```

Doctor checks:

- SQLite availability and `PRAGMA quick_check` integrity
- Firefox/Zen native-messaging manifest and native-host executable
- recent Firefox/Zen semantic adapter state
- recent VS Code semantic adapter state
- Git availability
- Docker availability/context
- persistent log directory availability

Missing optional/live integrations are warnings where Context Capsule can still operate partially. Corrupt local state or an invalid native-host installation is reported as an error.

## Diagnostics and logs

Persistent component logs are bounded and rotated instead of growing forever. Each component keeps its current log plus one previous file and normalizes control characters so one event cannot forge extra log records.

On Windows, logs live under:

```text
%LOCALAPPDATA%\ContextCapsule\logs\
```

Important files include:

```text
cli.log
firefox.log
vscode-host-<pid>.log
vscode-host-<pid>.log.1
```

The Firefox adapter deliberately logs lifecycle/outcome metadata such as window/tab counts and restore results rather than persisting captured tab URLs as diagnostics.

The default per-log bound is 1 MiB and an individual diagnostic message is capped at 4096 characters.

## Firefox / Zen native host

Install the native host from the built `capsule-firefox-host` executable:

```powershell
capsule-firefox-host --install
capsule-firefox-host --doctor
```

The Firefox/Zen extension uses the native host for local semantic-state synchronization, CLI restore requests, safe Zen blank-window creation, and persistent Firefox diagnostics.

## Safety model

Context Capsule restore is intentionally conservative:

- reuse already-satisfied state instead of duplicating it;
- do not mutate a changed live Zen window just because it shares a few tabs;
- do not guess ambiguous legacy terminal ownership;
- preserve old capsule revisions instead of destructively overwriting them;
- keep browser private windows out of capture;
- avoid replaying shell history as commands;
- prefer partial restore plus warnings to arbitrary reconstruction.

Use `--dry-run` before restoration when you want to inspect the plan first.
