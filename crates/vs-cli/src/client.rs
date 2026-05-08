//! Synchronous local-socket client for the daemon wire protocol.
//!
//! The CLI is short-lived; one connection per invocation, no async.
//! [`Client`] connects (AF_UNIX socket on Unix, named pipe on
//! Windows — both via [`interprocess`]), sends one request line,
//! reads response lines up to and including the blank-line
//! terminator, and exposes the parsed warnings + envelope + body to
//! the caller.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use interprocess::local_socket::{prelude::*, Stream};
use vs_protocol::{Envelope, Request, ResponseHead, Warning};

/// One CLI ↔ daemon connection.
pub struct Client {
    socket: PathBuf,
    stream: BufReader<Stream>,
}

impl Client {
    /// Connect to the daemon at `socket`. Returns the connected client
    /// or an error if the socket is missing / unreachable.
    pub fn connect(socket: impl Into<PathBuf>) -> Result<Self> {
        let socket = socket.into();
        let name = vs_daemon::transport::path_to_name(&socket)
            .with_context(|| format!("derive ipc name for {}", socket.display()))?;
        let stream =
            Stream::connect(name).with_context(|| format!("connect {}", socket.display()))?;
        Ok(Self {
            socket,
            stream: BufReader::new(stream),
        })
    }

    /// Connect, retrying for `timeout` if the socket is missing —
    /// useful immediately after spawning the daemon.
    pub fn connect_with_retry(socket: impl AsRef<Path>, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        let mut last_err = anyhow::anyhow!("connect: socket missing");
        loop {
            match Self::connect(socket.as_ref()) {
                Ok(c) => return Ok(c),
                Err(e) => {
                    last_err = e;
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        Err(last_err)
    }

    /// Send `req` and read one full response.
    pub fn call(&mut self, req: &Request) -> Result<Response> {
        // Send.
        let line = req.encode();
        self.stream
            .get_mut()
            .write_all(line.as_bytes())
            .context("write request")?;
        self.stream.get_mut().flush().context("flush request")?;

        // Read until blank-line terminator.
        let mut warnings: Vec<Warning> = Vec::new();
        let mut envelope: Option<Envelope> = None;
        let mut body_lines: Vec<String> = Vec::new();
        loop {
            let mut buf = String::new();
            let n = self.stream.read_line(&mut buf).context("read line")?;
            if n == 0 {
                anyhow::bail!("daemon closed connection");
            }
            // Strip trailing \n (or \r\n defensively).
            if buf.ends_with('\n') {
                buf.pop();
                if buf.ends_with('\r') {
                    buf.pop();
                }
            }
            if buf.is_empty() {
                if envelope.is_some() {
                    break;
                }
                continue;
            }
            if envelope.is_none() {
                if let Some(rest) = buf.strip_prefix('?') {
                    let _ = rest;
                    warnings.push(Warning::parse(&buf)?);
                    continue;
                }
                if buf.starts_with('@') || buf.starts_with('!') {
                    envelope = Some(Envelope::parse(&buf)?);
                    continue;
                }
                anyhow::bail!("expected ?/@/! envelope, got: {buf}");
            }
            body_lines.push(buf);
        }

        Ok(Response {
            warnings,
            envelope: envelope.expect("breaks only after envelope"),
            body: body_lines,
        })
    }

    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

/// One full response: warnings + envelope + body lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub warnings: Vec<Warning>,
    pub envelope: Envelope,
    pub body: Vec<String>,
}

impl Response {
    /// Return the response head, useful when the body is irrelevant
    /// (e.g. for re-encoding to print to the user).
    #[must_use]
    pub fn head(&self) -> ResponseHead {
        ResponseHead {
            warnings: self.warnings.clone(),
            envelope: self.envelope.clone(),
        }
    }

    /// True if the envelope is a success.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self.envelope, Envelope::Success(_))
    }

    /// Re-emit the response in canonical wire form, including the
    /// trailing blank line. Used by `--json=off` (the default) to
    /// stream the daemon's response to stdout verbatim.
    #[must_use]
    pub fn render_wire(&self) -> String {
        let mut out = self.head().encode();
        for line in &self.body {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        out
    }
}
