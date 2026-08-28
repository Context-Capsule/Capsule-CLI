pub mod docker;
pub mod terminal;

// ShellKind is a fieldless classification enum. Making it Copy keeps borrowed
// terminal-session matching/filtering ergonomic without cloning or moving state.
impl Copy for terminal::ShellKind {}
