// Several Windows-only modules bind the same Win32 functions using their own
// repr(C) mirror structs. Those declarations intentionally share the native ABI
// even though Rust gives the mirror structs distinct module-local types.
#![allow(clashing_extern_declarations)]

pub mod adapters;
pub mod browser;
pub mod desktop;
pub mod explorer;
pub mod persistence;
pub mod restore;
pub mod restore_bus;
pub mod vscode;
