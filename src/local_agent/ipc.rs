use crate::local_agent::{AgentError, protocol::MAX_IPC_MESSAGE_BYTES};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    net::TcpStream,
};

pub fn write_message<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<(), AgentError> {
    let mut encoded = serde_json::to_vec(value)
        .map_err(|error| AgentError::Protocol(format!("could not encode IPC message: {error}")))?;
    if encoded.len() > MAX_IPC_MESSAGE_BYTES {
        return Err(AgentError::Protocol(format!(
            "IPC message exceeds the {} byte limit",
            MAX_IPC_MESSAGE_BYTES
        )));
    }
    encoded.push(b'\n');
    stream.write_all(&encoded)?;
    stream.flush()?;
    Ok(())
}

pub fn read_message<T: DeserializeOwned>(stream: &mut TcpStream) -> Result<T, AgentError> {
    let mut payload = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(AgentError::Protocol(
                "IPC peer closed the connection before sending a complete message".to_owned(),
            ));
        }

        let bytes = &chunk[..read];
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            payload.extend_from_slice(&bytes[..newline]);
            break;
        }
        payload.extend_from_slice(bytes);

        if payload.len() > MAX_IPC_MESSAGE_BYTES {
            return Err(AgentError::Protocol(format!(
                "IPC message exceeds the {} byte limit",
                MAX_IPC_MESSAGE_BYTES
            )));
        }
    }

    if payload.len() > MAX_IPC_MESSAGE_BYTES {
        return Err(AgentError::Protocol(format!(
            "IPC message exceeds the {} byte limit",
            MAX_IPC_MESSAGE_BYTES
        )));
    }

    serde_json::from_slice(&payload)
        .map_err(|error| AgentError::Protocol(format!("invalid IPC JSON: {error}")))
}
