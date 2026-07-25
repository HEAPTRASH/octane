//! Transports.
//!
//! stdio is implemented here: spawn the server as a child process and exchange
//! newline-delimited JSON over its pipes. Streamable HTTP is the other transport
//! in the spec and slots in behind the same trait.

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

use crate::McpError;

#[async_trait]
pub trait Transport: Send + Sync {
    /// Send one message.
    async fn send(&self, message: &str) -> Result<(), McpError>;

    /// Await the next message. `None` means the peer closed cleanly.
    async fn recv(&self) -> Result<Option<String>, McpError>;

    /// Close per the spec: stdin first, then wait, then signal.
    async fn shutdown(&self) -> Result<(), McpError>;
}

/// A server run as a child process.
pub struct StdioTransport {
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    child: Mutex<Child>,
}

impl std::fmt::Debug for StdioTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioTransport").finish_non_exhaustive()
    }
}

impl StdioTransport {
    /// Spawn `program` with `args`.
    ///
    /// `env` is applied on top of the inherited environment rather than replacing
    /// it: a server almost always needs `PATH` and `HOME`, and an empty
    /// environment produces failures that look like the server is broken.
    pub async fn spawn(
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self, McpError> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Inherited so a server's diagnostics reach the user's terminal
            // instead of filling an unread pipe until it blocks.
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);

        for (key, value) in env {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .map_err(|error| McpError::Transport(format!("spawning {program}: {error}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdout".into()))?;

        Ok(Self {
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            child: Mutex::new(child),
        })
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send(&self, message: &str) -> Result<(), McpError> {
        let mut stdin = self.stdin.lock().await;
        let framed = format!("{message}\n");
        stdin
            .write_all(framed.as_bytes())
            .await
            .map_err(|error| McpError::Transport(error.to_string()))?;
        stdin.flush().await.map_err(|error| McpError::Transport(error.to_string()))?;
        Ok(())
    }

    async fn recv(&self) -> Result<Option<String>, McpError> {
        let mut stdout = self.stdout.lock().await;
        let mut line = String::new();

        match stdout.read_line(&mut line).await {
            Ok(0) => Ok(None),
            // Blank lines between messages are tolerated rather than treated as
            // protocol errors; some servers emit them.
            Ok(_) if line.trim().is_empty() => Ok(Some(String::new())),
            Ok(_) => Ok(Some(line.trim_end().to_string())),
            Err(error) => Err(McpError::Transport(error.to_string())),
        }
    }

    async fn shutdown(&self) -> Result<(), McpError> {
        // Spec order: close stdin, give the server a chance to exit, then signal.
        {
            let mut stdin = self.stdin.lock().await;
            let _ = stdin.shutdown().await;
        }

        let mut child = self.child.lock().await;
        let graceful =
            tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;

        if graceful.is_err() {
            let _ = child.kill().await;
        }
        Ok(())
    }
}
