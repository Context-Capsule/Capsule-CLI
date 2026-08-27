pub mod client;
pub mod protocol;
pub mod server;

mod components;
mod ipc;
mod paths;

use std::{error::Error, fmt};

#[derive(Debug)]
pub enum AgentError {
    Io(std::io::Error),
    Protocol(String),
    Runtime(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
            Self::Runtime(message) => formatter.write_str(message),
        }
    }
}

impl Error for AgentError {}

impl From<std::io::Error> for AgentError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
