// The worker includes the mature CLI modules directly. Several Windows-only
// modules bind the same Toolhelp functions using their own repr(C) mirror
// structs; those declarations share the native ABI even though Rust gives the
// mirror structs distinct module-local types. The library crate already carries
// this allowance; the standalone worker crate needs the same crate-level policy.
#![allow(clashing_extern_declarations)]
// The compatibility worker includes the full mature CLI module graph, so some
// library/test entry points are intentionally unreachable from this binary.
// Keep the allowance local to this worker instead of suppressing dead-code
// diagnostics for the library or public CLI crate.
#![allow(dead_code)]

#[cfg(windows)]
mod windows_snap;

include!("main.rs");