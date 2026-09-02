import subprocess


def run(*args: str) -> None:
    subprocess.run(args, check=True)


# Reuse the already-reviewed final patch script. This staging workflow checks out
# the latest branch ref, so this validates exactly what will be committed.
run("python", ".github/scripts/finalize_portrait_pair_restore.py")
run("python", ".github/scripts/restore_portrait_live_entrypoint.py")

# Format only files touched by the final patch; do not churn unrelated legacy
# Rust files.
run(
    "rustfmt",
    "--edition",
    "2024",
    "src/windows_snap_baseline.rs",
    "src/windows_snap_coord.rs",
    "src/restore/windows.rs",
    "src/restore/custom_snap.rs",
    "src/restore/mod.rs",
    "tests/windows_snap_live.rs",
)

# Full hosted-Windows regression gate. The one skipped test is an existing live
# terminal-value test that is intentionally excluded by the repository's focused
# Windows validation jobs as well.
run(
    "cargo",
    "test",
    "--all-targets",
    "--",
    "--skip",
    "git_context::tests::live_terminal_value_contributes_repo_omitted_from_durable_terminal_snapshot",
)

# Only after the full suite passes, commit the six product/test files. Persisted
# checkout credentials let this old staging harness push the latest branch safely.
run("git", "config", "user.name", "github-actions[bot]")
run(
    "git",
    "config",
    "user.email",
    "41898282+github-actions[bot]@users.noreply.github.com",
)
run(
    "git",
    "add",
    "src/windows_snap_baseline.rs",
    "src/windows_snap_coord.rs",
    "src/restore/windows.rs",
    "src/restore/custom_snap.rs",
    "src/restore/mod.rs",
    "tests/windows_snap_live.rs",
)

cached = subprocess.run(["git", "diff", "--cached", "--quiet"]).returncode
if cached != 0:
    run("git", "commit", "-m", "test: retain portrait live regression entrypoint")
    run("git", "push", "origin", "HEAD:fix/portrait-stacked-snap-20260902")
