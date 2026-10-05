//! Bounded newline framing. Stdout contains JSON-RPC messages exclusively.
use crate::{McpError, McpServer, Result, protocol::Protocol};
use std::io::{BufRead, Read, Write};
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// Runs one initialized stdio session until EOF. The caller owns the MCP lock.
/// # Errors
/// I/O errors and frames exceeding the limit terminate with a safe label.
pub fn run(server: &McpServer, input: impl BufRead, mut output: impl Write) -> Result<()> {
    server.with_startup_recovery(|| run_session(server, input, &mut output))
}
fn run_session(server: &McpServer, input: impl BufRead, mut output: impl Write) -> Result<()> {
    let mut input = input;
    let mut protocol = Protocol::default();
    loop {
        let mut bytes = Vec::new();
        let count = Read::take(&mut input, (MAX_MESSAGE_BYTES + 1) as u64)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| McpError::Io)?;
        if count == 0 {
            return Ok(());
        }
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(McpError::FrameTooLarge);
        }
        // EOF without a delimiter is tolerated for the final message.
        if let Some(response) = protocol.handle_bytes(server, &bytes) {
            serde_json::to_writer(&mut output, &response).map_err(|_| McpError::Io)?;
            output
                .write_all(b"\n")
                .and_then(|()| output.flush())
                .map_err(|_| McpError::Io)?;
        }
    }
}
