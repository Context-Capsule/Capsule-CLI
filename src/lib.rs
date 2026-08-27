// Several Windows-only modules bind the same Win32 functions using their own
// repr(C) mirror structs. Those declarations intentionally share the native ABI
// even though Rust gives the mirror structs distinct module-local types.
#![allow(clashing_extern_declarations)]

extern crate self as context_capsule;

pub mod adapters;
pub mod browser;
pub mod chrome;
pub mod cleanup;
pub mod desktop;
pub mod diagnostics;
pub mod diff;
pub mod explorer;
#[path = "git_context_v3.rs"]
pub mod git_context;
pub mod logging;
pub mod persistence;
pub mod restore;
pub mod restore_bus;
pub(crate) mod terminal_context;
pub mod vscode;

#[cfg(windows)]
#[path = "zen_shortcuts_hardened.rs"]
pub(crate) mod zen_shortcuts;
#[cfg(windows)]
#[path = "zen_shortcuts.rs"]
pub(crate) mod zen_shortcuts_core;

#[cfg(windows)]
#[path = "windows_snap_safe.rs"]
pub(crate) mod windows_snap;
#[cfg(windows)]
#[path = "windows_snap_coord.rs"]
pub(crate) mod windows_snap_coord;
#[cfg(windows)]
#[path = "windows_snap_drag.rs"]
pub(crate) mod windows_snap_drag;
#[cfg(windows)]
#[path = "windows_snap.rs"]
pub(crate) mod windows_snap_core;
#[cfg(windows)]
#[path = "windows_snap_baseline.rs"]
pub(crate) mod windows_snap_legacy;

// CI rerun trigger for self-hosted runner validation.
