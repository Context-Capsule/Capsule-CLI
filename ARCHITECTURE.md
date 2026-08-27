# Context Capsule Architecture

## Local Agent boundary

Context Capsule uses a local process boundary between user-facing clients and workspace state machinery:

```text
CLI
 │
 │ authenticated loopback IPC (protocol v1)
 ▼
Local Agent
 ├── capture engine
 ├── restore engine
 ├── SQLite service
 ├── adapter host
 └── IPC server
```

The CLI is intentionally a thin client. It does not directly open the capsule database, discover workspace state, restore windows, or talk to browser/editor adapters. Every normal command is sent to the Local Agent.

### Process model

`capsule` has two roles inside the same installed executable:

1. **CLI client** — the normal process started by the user.
2. **Local Agent server** — a hidden internal mode started automatically on first use.

The Local Agent owns an internal `capsule-agent-worker` process boundary. The worker currently hosts the mature command/capture/restore implementation unchanged. Keeping that tested implementation behind the new agent boundary is deliberate: it lets the architecture change without rewriting working restore logic in the same change.

The boundary is now stable enough for capture, restore, SQLite, or individual adapters to be moved from the compatibility worker directly into the agent later without changing the public CLI or IPC contract.

### Agent components

- **Capture engine** owns discovery-oriented commands such as `inspect`, `save`, `update`, terminal inspection, and Docker inspection.
- **Restore engine** owns full/partial capsule restore and Docker restore.
- **SQLite service** owns state/history-oriented commands such as `list`, `history`, `show`, `note`, `diff`, and `delete`.
- **Adapter host** owns the worker/native-adapter execution boundary and compatibility commands such as `doctor` and help/error rendering.
- **IPC server** authenticates and serializes local requests before they reach any component.

The agent processes requests serially. That avoids introducing new concurrent SQLite/capture/restore races while the architecture is being extracted.

## IPC contract

The first protocol is a newline-delimited JSON request/response protocol over a dynamically allocated `127.0.0.1` TCP port.

Important invariants:

- The listener binds only to IPv4 loopback, never to a LAN/public interface.
- A per-agent token is stored beside the agent state and must match on every request.
- On Unix, the state file is written with `0600` permissions. On Windows it lives under the user's normal Context Capsule application-data directory and inherits that directory's user ACL.
- Requests and responses carry a protocol version and request ID.
- IPC messages have a size ceiling.
- The CLI forwards its current working directory and Unicode environment variables so capture, Git, WSL, custom DB paths, PATH resolution, and adapter behavior observe the same caller context as before the agent boundary.
- The response preserves the worker's stdout, stderr, and exit code.
- The agent records only request IDs, routed subsystem, and exit status in its local log; command arguments and authentication tokens are not logged.

## Lifecycle

The agent starts lazily on the first normal `capsule` command. It writes a small state file containing its PID, loopback port, protocol version, executable stamp, and authentication token.

The CLI checks that:

1. the state file parses,
2. the protocol version matches,
3. the endpoint responds to an authenticated ping,
4. the responding PID matches the state file, and
5. the running agent was started from the current executable build.

If the `capsule` executable is replaced during an upgrade, a new CLI shuts down the stale agent instead of silently sending commands to old code.

Management commands:

```text
capsule agent start
capsule agent status
capsule agent stop
capsule agent restart
```

The state/lock files are removed on graceful shutdown. If a process crashes, a later CLI waits briefly for an in-progress startup and then removes stale runtime files before starting a replacement.

## Worker compatibility boundary

The internal worker exists to make this architectural change low-risk. Existing command parsing, capture, restore, selective `--only` behavior, continuation notes, Git dirty-worktree safety, window placement, native messaging, and SQLite schemas remain in their existing implementation.

In installed/release layouts, `capsule-agent-worker` must be shipped beside `capsule`. During debug `cargo run` development and CI, the agent invokes the worker through the same Cargo manifest so worker-owned source changes cannot be masked by a stale previously-built worker. Release/install builds use the sibling worker directly.

This compatibility worker is not a second public CLI. It is an implementation detail behind the Local Agent and can be removed incrementally as the individual engines are moved in-process.

## Regression rule

A Local Agent refactor must not change public command behavior merely because the process boundary changed. In particular:

- normal command names/arguments remain the same,
- `--only` selective restore semantics remain the same,
- output intended for scripts (especially `show --json`) remains on stdout without agent chatter,
- errors remain on stderr,
- command exit codes remain preserved,
- the caller CWD/environment is replayed in the worker,
- persisted snapshot schemas are not changed by the agent itself, and
- browser/editor/terminal/native restore implementations remain behind their existing tested interfaces.
