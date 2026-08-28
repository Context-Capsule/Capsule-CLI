// The worker includes the mature CLI modules directly. Several Windows-only
// modules bind the same Toolhelp functions using their own repr(C) mirror
// structs; those declarations share the native ABI even though Rust gives the
// mirror structs distinct module-local types. The library crate already carries
// this allowance; the standalone worker crate needs the same crate-level policy.
#![allow(clashing_extern_declarations)]

#[cfg(windows)]
mod windows_snap;

include!("main.rs");
