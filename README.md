# Context Capsule CLI

Context Capsule captures the **working state around a project** and restores it semantically: Git context, developer tools, terminals, VS Code, Zen/Firefox, Chrome, Docker resources, desktop applications/windows, Explorer folders, and display placement where supported.

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

## Browser adapters

Firefox/Zen and Chrome use separate native host registrations and separate live-state/restore-bus channels. A capsule can contain either browser or both:

```json
{
  "browsers": {
    "firefox": { "browser": "firefox" },
    "chrome": { "browser": "chrome" }
  }
}
```

`browsers.chrome` is added only when a recent Chrome adapter state exists, so Firefox-only installations keep the historical capsule payload shape.

### Firefox / Zen native host

```powershell
cargo build --bin capsule-firefox-host
cargo run --bin capsule-firefox-host -- --install
cargo run --bin capsule-firefox-host -- --doctor
```

Host name:

```text
com.contextcapsule.host
```

The Firefox/Zen extension uses this host for local semantic-state synchronization, CLI restore requests, the safe Zen blank-window fallback, and persistent Firefox diagnostics.

### Chrome native host

Build/install the additive Chrome host:

```powershell
cargo build --bin capsule-chrome-host
cargo run --bin capsule-chrome-host -- --install
cargo run --bin capsule-chrome-host -- --doctor
```

Host name:

```text
com.contextcapsule.chrome
```

The Chrome development extension has the deterministic ID:

```text
gmffhdppfaeonombpbbgnldagfeabiof
```

On Windows the installer registers `com.contextcapsule.chrome` under Google Chrome's `NativeMessagingHosts` registry path and authorizes only that extension origin. Firefox's Mozilla registration is not modified.

The Chrome host uses its own runtime state (`chrome.json`), restore-bus adapter (`chrome`) and log (`chrome.log`). Normal `capsule restore` restores Firefox and Chrome independently when both snapshots exist.

## Save-time application exclusions

Applications can be excluded from a capsule with repeatable `--ignore-app` options:

```powershell
capsule save work --ignore-app Zen
capsule save work --ignore-app "Google Chrome"
capsule save work --ignore-app "Visual Studio Code" --ignore-app WindowsTerminal.exe
capsule save work --ignore-app=Code.exe
```

Selectors are case-insensitive and can match an application name, executable name, executable path, AppUserModelID, or saved launch target. Context Capsule rejects a selector that matches no currently discovered application rather than silently accepting a typo. Use `capsule apps` or `capsule inspect --apps` to see the application names Context Capsule currently detects.

Exclusions are also applied to application-owned semantic state so an ignored app cannot silently reappear through another adapter:

- ignoring Zen/Firefox suppresses the Firefox/Zen semantic snapshot;
- ignoring Google Chrome suppresses the Chrome semantic snapshot;
- ignoring VS Code suppresses its editor semantic snapshot and VS Code-hosted terminal sessions;
- ignoring Windows Terminal suppresses its terminal sessions and stored Windows Terminal layouts;
- ignoring File Explorer suppresses folder-window restore state;
- Docker/Compose resources remain independent of Docker Desktop, so ignoring Docker Desktop does not delete container/Compose state from the capsule.

Resolved ignored application names are stored under `capture_options.ignored_applications` and are shown by `capsule show`. `capsule update <name>` inherits those exclusions into the next revision.

## Terminal ownership and working directories

The shell process hosting the active `capsule save` or `capsule update` command is capture infrastructure, not workspace state. On Windows, Context Capsule walks the current command's process ancestry and removes that hosting shell from the generic terminal snapshot before it is stored. This applies even when the shell is exposed through an intermediate console/PTTY host.

VS Code integrated terminals are owned by the VS Code semantic adapter. When a recent VS Code semantic snapshot is available, generic process-derived VS Code terminal entries are not stored as independent restart plans; the editor snapshot retains the integrated terminal name, shell, arguments, active state and CWD. This prevents the terminal used to invoke Context Capsule from reappearing as a second standalone console during restore.

For standalone Windows shells such as `cmd.exe`, PowerShell and `pwsh`, Context Capsule attempts a bounded read of the live process current directory. If Windows permits the query, the path is stored both as the terminal's `working_directory` and as `restart.working_directory`. Restore already launches direct shell restart plans with `Command::current_dir`, so the shell is recreated in the saved directory rather than the platform default directory.

The process-CWD probe is fail-open: permission, bitness or process-lifetime failures leave the CWD unknown instead of failing the capsule save. Restore-time terminal matching uses the same CWD enrichment, so an already-open shell in the correct directory is reused while a same-type shell in a different directory does not incorrectly satisfy the saved session.

Capsules saved before this behavior cannot retroactively recover a standalone shell CWD that was never stored; save or update the capsule with a current build to capture it.

## Saved display setup

Every new save/update now stores a versioned `display_setup` snapshot in addition to the existing per-window/per-display desktop placement metadata. It records:

- display count and primary display;
- each display's pixel bounds and work area;
- scale percentage and orientation;
- relation to the primary display;
- the union/virtual desktop bounds;
- a geometry-oriented topology signature that is stable across discovery ordering;
- a device-and-geometry signature for stricter future comparisons.

Example shape:

```json
{
  "display_setup": {
    "schema_version": 1,
    "status": "available",
    "display_count": 2,
    "primary_device": "DISPLAY1",
    "virtual_bounds": {
      "left": -1920,
      "top": 0,
      "right": 2560,
      "bottom": 1440
    },
    "topology_signature": "...",
    "device_signature": "...",
    "displays": []
  }
}
```

Restore does not yet reject or remap a capsule based on these signatures. The metadata is intentionally captured now so a later restoreability phase can compare the current monitor topology with the saved topology and decide when exact window placement is impossible or needs a fallback mapping.

## Restore modes: append vs replace

Restore remains backward compatible: **append mode is the default**. It preserves unrelated running applications and applies the capsule on top of the current desktop, just as Context Capsule did before replace mode existed.

```powershell
capsule restore work
capsule restore work --append
```

Use `--replace` when the current desktop should be cleaned before restoration:

```powershell
capsule restore work --replace
```

`--close-unrelated` is an alias for `--replace`.

Replace mode first discovers the current user applications using the same desktop classifier used for capture. Applications whose strong identity belongs to the capsule are preserved. Unrelated applications receive a normal Windows close request first; Context Capsule then re-discovers the desktop and force-terminates unrelated non-shell applications that are still alive. The final desktop is verified before the normal restore engine is allowed to start.

Explorer is handled specially because File Explorer folder windows share the same `explorer.exe` process as the Windows desktop shell. Replace mode sends `WM_CLOSE` directly to unrelated Explorer folder windows while explicitly leaving the `Program Manager` shell window and `explorer.exe` process alive.

Packaged/UWP applications whose user-facing window is owned by `ApplicationFrameHost.exe` are also included in replace cleanup. Context Capsule compares those visible surfaces with the capsule's saved ignored-window inventory, closes newly introduced surfaces with `WM_CLOSE`, and never kills the shared Application Frame Host process itself.

Replace cleanup is intentionally strict:

- the terminal/editor process chain hosting the active `capsule` command is always protected;
- Docker Desktop is preserved when the capsule contains Docker/Compose resources;
- `explorer.exe` itself is never terminated as a cleanup side effect;
- unrelated normal applications that ignore graceful shutdown are force-terminated because `--replace` explicitly requests a clean application set;
- if an unrelated application or packaged-app window is still present after cleanup, replace mode fails and restoration does not continue as an accidental append;
- if Context Capsule cannot establish or verify the cleanup inventory, restoration does not start.

`--replace` can therefore discard unsaved work in an unrelated application if that application refuses the initial graceful close. Use the dry run first when the current desktop may contain work you want to keep:

```powershell
capsule restore work --replace --dry-run
```

After cleanup, replace mode invokes the same restore engine as append mode. Window placement, native Snap restoration, Firefox/Zen and Chrome semantic restoration, VS Code restoration, terminal restoration, Explorer restoration, and Docker behavior are not forked into a second restore implementation.

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

Current diff sections include workspace/system context, Git, Firefox/Zen tabs and named groups, VS Code workspace/tabs/integrated terminals, external terminal sessions, Docker resources, desktop applications, and developer tool versions. Chrome capture/restore is supported independently; Chrome-specific semantic diff output can be generalized in a later diff-format change without affecting restore.

Duplicate browser/editor tabs are treated as a multiset, so adding or removing one copy is represented once instead of collapsing duplicates accidentally.

## Doctor

```powershell
capsule doctor
capsule doctor --verbose
capsule doctor --json
```

The general doctor checks SQLite, the existing Firefox/Zen native integration, VS Code, Git, Docker and logging. Chrome's independent native registration has its own exact doctor:

```powershell
capsule-chrome-host --doctor
```

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
chrome.log
vscode-host-<pid>.log
vscode-host-<pid>.log.1
```

The browser adapters deliberately log lifecycle/outcome metadata such as window/tab counts and restore results rather than persisting captured tab URLs as diagnostics.

The default per-log bound is 1 MiB and an individual diagnostic message is capped at 4096 characters.

## Safety model

Context Capsule restore is intentionally conservative by default:

- append mode reuses already-satisfied state instead of duplicating it;
- browser adapters use semantic tab/window topology instead of relying only on volatile active-tab window titles;
- do not guess ambiguous legacy terminal ownership;
- preserve old capsule revisions instead of destructively overwriting them;
- keep browser private/incognito windows out of capture;
- avoid replaying shell history as commands;
- prefer partial restore plus warnings to arbitrary reconstruction.

Replace mode is the explicit exception: its purpose is to remove unrelated applications before restoring the capsule, and it may force-close an unrelated app after a graceful close attempt. Use `--replace --dry-run` before restoration when you want to inspect that cleanup plan first.