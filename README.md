# Context Capsule CLI

Context Capsule captures the **working state around a project** and restores it semantically: Git context, developer tools, terminals, VS Code, Firefox/Zen, Chrome, Docker resources, desktop applications/windows, Explorer folders, and display placement where supported.

This repository is the **core engine** of the Context Capsule ecosystem. It owns the public CLI, Local Agent/worker boundary, persistence/revisions, operating-system discovery and restore behavior, browser native hosts, and the machine-readable API used by the desktop app.

Context Capsule is local-first: capsule data is stored on the machine in SQLite, while browser/editor adapters synchronize semantic state through local runtime files, native messaging, and restore-bus channels rather than a hosted service.

For deeper detail on the internal Local Agent extraction and IPC invariants, also read [`ARCHITECTURE.md`](./ARCHITECTURE.md).

## Context Capsule ecosystem

Context Capsule is split across four cooperating repositories:

```text
                               users / automation
                                      |
                        +-------------+-------------+
                        |                           |
                        v                           v
              +------------------+        +----------------------+
              | Desktop App      |        | capsule CLI client   |
              | Tauri + Svelte   |        | this repository      |
              +--------+---------+        +----------+-----------+
                       | bundled/allowed             |
                       | CLI operations              | authenticated loopback IPC
                       +-----------------------------+
                                                     v
                                           +--------------------+
                                           | Local Agent        |
                                           | this repository    |
                                           +---------+----------+
                                                     |
                                                     v
                                           +--------------------+
                                           | agent worker +     |
                                           | capture/restore    |
                                           | persistence        |
                                           +--+-------------+---+
                                              |             |
                     runtime state/restore bus|             |native messaging
                                              |             |
                      +-----------------------+             +----------------------+
                      |                                                            |
                      v                                                            v
           +------------------------+                                 +------------------------+
           | VS Code Extension      |                                 | Browser Extension      |
           | semantic editor state  |                                 | Firefox/Zen + Chrome   |
           +------------------------+                                 +------------------------+
```

### Repository responsibilities

| Repository | Owns | Typical integration contract |
| --- | --- | --- |
| **Capsule-CLI** | Public CLI, Local Agent/worker, SQLite/revisions, generic capture/restore, Windows desktop state, terminals, Docker, browser native hosts, desktop machine API | CLI args/output, local runtime schemas, native-message protocols, semantic snapshot schemas |
| [Capsule-Desktop-App](https://github.com/Context-Capsule/Capsule-Desktop-App) | Tray/full-app UX, Tauri allow-list, packaging the CLI runtime | `capsule desktop ...` JSON API + allowed mutations |
| [Capsule-Browser-Extension](https://github.com/Context-Capsule/Capsule-Browser-Extension) | Firefox/Zen + Chrome browser semantic capture/restore | native messaging + browser-specific runtime state/restore channels |
| [Capsule-VSCode-Extension](https://github.com/Context-Capsule/Capsule-VSCode-Extension) | VS Code workspace/editor/integrated-terminal semantic capture/restore | live runtime snapshot + VS Code restore bus |

## Where should a feature be implemented?

Start here when the feature changes **Context Capsule domain behavior** rather than only one client UI.

| Feature/change | Primary repository / area |
| --- | --- |
| New CLI command/flag/output | This repo: public command routing + `src/commands*` |
| New capsule field, schema, revision/history behavior | This repo: persistence/model/command layer; coordinate adapters that produce the field |
| Generic save/inspect/update behavior | This repo: capture/command engine |
| Generic restore/selective restore/replace cleanup | This repo: restore/cleanup engine |
| Windows applications, windows, Explorer, monitor placement | This repo: `src/desktop/`, related capture/restore modules |
| Standalone terminals or Docker/Compose | This repo: `src/adapters/terminal.rs`, `src/adapters/docker.rs` |
| Firefox/Zen or Chrome tab/window/group algorithm | `Capsule-Browser-Extension` first; this repo only for persistence/native host/routing contracts |
| Browser native-host executable, installation, doctor, registry/manifest behavior | This repo: `src/bin/firefox_host.rs`, `src/bin/chrome_host.rs` and browser runtime modules |
| VS Code tabs/workspace/selections/integrated-terminal semantics | `Capsule-VSCode-Extension`; this repo stores/routes its semantic snapshot |
| Desktop/tray UI | `Capsule-Desktop-App`; add/extend `capsule desktop` API here only if new engine data is required |
| Machine-readable desktop read model | This repo: `src/desktop_api.rs`, then update Desktop types/bridge/UI |
| A feature affecting all clients | Define behavior/contracts here first, then make adapters/UIs thin consumers |

A useful rule: **if a feature must work from the command line with no desktop app, browser popup, or VS Code command open, its domain behavior belongs here.**

## Internal architecture

The public CLI is deliberately separated from the stateful engine by a local process boundary:

```text
capsule.exe
public CLI client
     |
     | authenticated newline-delimited JSON
     | over dynamically allocated 127.0.0.1 port
     v
+----------------------------+
| Local Agent                |
| - lifecycle/state file     |
| - request validation       |
| - serial routing           |
| - caller CWD/environment   |
+-------------+--------------+
              |
              v
+----------------------------+
| capsule-agent-worker       |
| compatibility boundary     |
| mature command/capture/    |
| restore implementation     |
+------+------+--------------+
       |      |
       |      +----------------------+-------------------+
       |                             |                   |
       v                             v                   v
 SQLite/revisions             OS/tool adapters     browser/editor bridges
```

The worker is an internal compatibility boundary, not a second public CLI. Keeping mature behavior behind the Local Agent allows the architecture to evolve without rewriting working capture/restore behavior at the same time.

Important invariants include:

- agent listener binds to loopback only;
- every request uses the per-agent authentication token;
- protocol/request IDs and size limits are enforced;
- the caller's working directory/environment are forwarded so Git, WSL, PATH and custom configuration resolve as if the command ran directly;
- worker stdout/stderr/exit code are preserved for CLI compatibility;
- normal requests are serialized to avoid introducing capture/restore/SQLite races;
- replacing the installed executable causes stale-agent detection/restart rather than silently running old engine code.

See [`ARCHITECTURE.md`](./ARCHITECTURE.md) before changing the agent/worker/IPC boundary.

## Source map for developers

The repository is intentionally organized around a Rust core plus adapter/native-host entry points. High-value locations include:

```text
Capsule-CLI/
├─ src/
│  ├─ capsule_main.rs            public `capsule` binary entry point
│  ├─ agent_worker_main.rs       internal compatibility worker entry point
│  ├─ commands.rs / commands/    command parsing/routing and command-specific behavior
│  ├─ desktop_api.rs             versioned machine-readable API for Desktop App
│  ├─ diagnostics.rs             logging/diagnostic infrastructure
│  ├─ cleanup.rs                 replace-mode cleanup/safety behavior
│  ├─ desktop/
│  │  ├─ model.rs                desktop/window model
│  │  ├─ classify.rs             application/window classification
│  │  ├─ windows.rs              Windows-native discovery/operations
│  │  └─ dpi.rs                  DPI-related helpers
│  ├─ adapters/
│  │  ├─ terminal.rs             generic terminal capture/restore
│  │  └─ docker.rs               Docker/Compose adapter
│  ├─ browser.rs                 Firefox/Zen state/native integration support
│  ├─ browser_live.rs            live Firefox/Zen runtime state/restore-bus support
│  ├─ chrome.rs                  Chrome state/native integration support
│  └─ bin/
│     ├─ firefox_host.rs          `capsule-firefox-host`
│     └─ chrome_host.rs           `capsule-chrome-host`
├─ tests/                        integration/regression tests
├─ Cargo.toml                    binaries and dependencies
├─ ARCHITECTURE.md               Local Agent/worker/IPC architecture
└─ scripts/                      targeted diagnostics/maintenance helpers
```

When a source area grows, prefer focused modules over adding unrelated behavior to the public entry points.

## Binaries

`Cargo.toml` defines four binaries:

```text
capsule                 public CLI + Local Agent mode
capsule-agent-worker    internal worker used by the agent
capsule-firefox-host    Firefox/Zen native messaging host
capsule-chrome-host     Chrome native messaging host
```

Release/desktop packaging must keep the worker and required native hosts alongside the main CLI binary. The Desktop App deliberately packages these as a runtime set rather than treating `capsule.exe` as a standalone executable.

## Build from source

### Requirements

- Rust stable toolchain with Cargo
- Windows 10/11 for the complete Windows-native capture/restore feature set
- Git for Git-context features
- optional: Docker for Docker/Compose features
- optional: the browser/VS Code adapters for their semantic integrations

### Debug build

```powershell
cargo build --bins
```

### Release build

```powershell
cargo build --release --bins
```

Release outputs are under:

```text
target\release\
```

### Validation

Run the Rust regression suite:

```powershell
cargo test
```

Compile all targets without running them:

```powershell
cargo check --all-targets
```

For a cross-repo protocol/schema feature, also run the affected adapter repository's test/check command and perform a real end-to-end save/restore cycle.

## Running during development

Use `cargo run --` in place of an installed `capsule` binary:

```powershell
# Inspect the current working state
cargo run -- inspect --verbose

# Save the first revision
cargo run -- save work

# Save a new immutable revision
cargo run -- update work

# Show revision history
cargo run -- history work

# Compare revisions
cargo run -- diff work@1 work@2

# Preview a restore without changing the machine
cargo run -- restore work@1 --dry-run

# Restore it
cargo run -- restore work@1

# Diagnose local integrations
cargo run -- doctor --verbose
```

After installing/building and placing the runtime directory on `PATH`, replace `cargo run --` with `capsule`.

## Core user workflow

```powershell
capsule inspect --verbose
capsule save work
capsule update work
capsule history work
capsule show work
capsule diff work@1 work@2
capsule restore work --dry-run
capsule restore work
capsule doctor --verbose
```

A capsule name resolves to its latest revision. Historical revisions remain addressable as `name@revision`.

## Local Agent lifecycle

The Local Agent starts lazily on the first normal command. Management commands are available for development and diagnostics:

```powershell
capsule agent start
capsule agent status
capsule agent stop
capsule agent restart
```

The CLI validates agent protocol version, authentication, responding PID and executable build identity before sending normal work. Stale/crashed runtime state is recovered on subsequent invocations.

## Desktop App contract

The Desktop App is a thin UI over this engine. Machine-readable reads are exposed under the `desktop` command namespace, including:

```text
capsule desktop contract
capsule desktop overview
capsule desktop capsule <name[@revision]>
capsule desktop history <name>
capsule desktop diff <before> <after>
capsule desktop live
capsule desktop health
capsule desktop services <name[@revision]>
capsule desktop log-paths
```

Responses use a versioned JSON envelope with `api_version`, `ok`, and either `data` or `error`.

When adding a GUI-visible engine feature:

1. implement/test the domain behavior here;
2. extend the machine-readable desktop contract here if necessary;
3. update Desktop `src/lib/types.ts` and `src/lib/bridge.ts`;
4. add the Svelte UI last.

Do not make the Desktop App parse human CLI output when a stable structured API is appropriate.

## Browser integration

Firefox/Zen and Chrome use separate native hosts and runtime channels.

### Firefox / Zen

Build/install/check the host:

```powershell
cargo build --bin capsule-firefox-host
cargo run --bin capsule-firefox-host -- --install
cargo run --bin capsule-firefox-host -- --doctor
```

Host name:

```text
com.contextcapsule.host
```

The WebExtension implementation lives in [Capsule-Browser-Extension](https://github.com/Context-Capsule/Capsule-Browser-Extension). This repository owns the native executable/installation, runtime channel, persistence and restore routing.

### Chrome

```powershell
cargo build --bin capsule-chrome-host
cargo run --bin capsule-chrome-host -- --install
cargo run --bin capsule-chrome-host -- --doctor
```

Host name:

```text
com.contextcapsule.chrome
```

The Chrome development extension uses deterministic ID:

```text
gmffhdppfaeonombpbbgnldagfeabiof
```

Chrome uses its own runtime state/channel/log rather than modifying the Firefox integration. Normal `capsule restore` can restore both browser snapshots independently when both are present.

### Browser protocol development rule

If the extension's `src/native/protocol.ts` changes, update the matching Rust host/runtime code in this repository in the same coordinated feature and validate both sides together. Never silently change one side of the native-message contract.

## VS Code integration

The VS Code semantic adapter is maintained in [Capsule-VSCode-Extension](https://github.com/Context-Capsule/Capsule-VSCode-Extension).

The extension continuously writes an atomic live snapshot under the Context Capsule runtime directory. Save/update consumes recent semantic state; restore publishes a request to the matching extension-host restore bus. A closed matching Extension Development Host can be relaunched before the restore request is consumed when safely identifiable.

VS Code-integrated terminal semantics belong to the VS Code extension. Generic standalone terminal behavior belongs to this repository. Keep that ownership boundary to avoid duplicate terminal capture/restart.

## Save-time application exclusions

Applications can be excluded with repeatable `--ignore-app` selectors:

```powershell
capsule save work --ignore-app Zen
capsule save work --ignore-app "Google Chrome"
capsule save work --ignore-app "Visual Studio Code" --ignore-app WindowsTerminal.exe
capsule save work --ignore-app=Code.exe
```

Selectors are case-insensitive and can match application name, executable name/path, AppUserModelID, or saved launch target. A selector that matches no currently discovered application is rejected rather than silently stored as a typo.

Use:

```powershell
capsule apps
capsule inspect --apps
```

to inspect discoverable names.

Exclusions also suppress semantic adapter state for the ignored application. For example, ignoring VS Code suppresses its editor snapshot and VS Code-owned terminals; ignoring a supported browser suppresses that browser's semantic snapshot.

## Terminal ownership

The shell hosting the active `capsule save`/`update` command is capture infrastructure, not workspace state, and is excluded from the generic terminal snapshot.

VS Code integrated terminals are owned by the VS Code semantic adapter when a recent adapter snapshot exists. Standalone terminal discovery/restart belongs to `src/adapters/terminal.rs`.

For supported standalone Windows shells, Context Capsule attempts a bounded process-CWD read and uses the resulting directory to improve both saved restart metadata and restore-time matching. Failure to read a CWD is fail-open rather than a reason to fail the entire capsule save.

## Restore modes

Append mode is the default and preserves unrelated current applications:

```powershell
capsule restore work
capsule restore work --append
```

Replace mode explicitly cleans unrelated application state before restoration:

```powershell
capsule restore work --replace
```

`--close-unrelated` is an alias for `--replace`.

Preview replace cleanup before applying it:

```powershell
capsule restore work --replace --dry-run
```

Replace mode uses the same underlying restore engine after cleanup; it does not maintain a second independent implementation for window placement, browser/VS Code semantics, terminals, Explorer, or Docker.

Because replace mode may force-close an unrelated application after a graceful-close attempt, it can discard unsaved work in that unrelated application. Keep its preflight/verification safety rules strict.

## Immutable revisions

A capsule name points to the latest revision, while historical revisions remain immutable and addressable:

```text
work        latest revision
work@1      first saved state
work@2      second saved state
```

Both of these create a new revision when `work` already exists:

```powershell
capsule update work
capsule save work --force
```

Deleting a capsule deletes the capsule and its revision history. Individual historical revisions are not edited in place.

## Semantic diff

Compare revisions with:

```powershell
capsule diff work@2 work@5
capsule diff work@2 work@5 --json
```

Diff is intended to compare semantic meaning rather than raw JSON ordering. Current areas include project/system context, Git, browser/editor state, terminals, Docker resources, desktop applications, and developer-tool versions where supported.

## Doctor and diagnostics

Run:

```powershell
capsule doctor
capsule doctor --verbose
capsule doctor --json
```

Browser-native registration also has exact host-specific doctor commands:

```powershell
capsule-firefox-host --doctor
capsule-chrome-host --doctor
```

Missing optional integrations should remain warnings when Context Capsule can operate partially; corrupt local state or invalid required integration should be surfaced as errors.

On Windows, Context Capsule logs live under:

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

Component logs are bounded/rotated and normalize control characters. Browser diagnostics intentionally record lifecycle/outcome metadata rather than captured tab URLs.

## Developing a cross-repo feature

Use this sequence to keep ownership clear:

1. **Define the domain behavior here** if the feature changes what a capsule means, stores, captures, restores, diagnoses, or exposes.
2. Add/update Rust tests and CLI behavior.
3. Define any structured contract change explicitly: desktop API, browser native messages/runtime schema, or VS Code runtime/restore-bus schema.
4. Update the producing/consuming adapter repository.
5. Update Desktop only if a GUI surface is needed.
6. Build/test each affected repository.
7. Run an end-to-end scenario from real capture through persistence to restore.

Examples:

- New Chrome tab semantic -> browser extension model/capture/restore + CLI persistence/protocol only if needed.
- New VS Code semantic -> VS Code adapter + CLI storage/routing only if needed.
- New Windows window-placement rule -> CLI only, then Desktop merely displays/configures it if desired.
- New GUI-only layout -> Desktop only; do not change the engine.

## Safety model

Context Capsule restore is conservative by default:

- append mode reuses already-satisfied state instead of duplicating it;
- browser/editor adapters restore semantic state instead of relying only on volatile process/window titles;
- ambiguous terminal ownership is not guessed;
- private/incognito browser windows stay out of capture;
- shell history is not replayed as commands;
- old capsule revisions are preserved rather than destructively overwritten;
- partial safe restore plus warnings is preferred to arbitrary reconstruction.

Replace mode is the explicit destructive exception and must remain opt-in, preflightable with `--dry-run`, and strict about protecting the active Context Capsule command chain and Windows shell-critical state.

These boundaries are architecture, not just implementation details. Preserve them when adding features.