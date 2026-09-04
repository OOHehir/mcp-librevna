//! Newline-framed SCPI client for the LibreVNA-GUI TCP server.
//!
//! The GUI serves SCPI on port 19542 by default and accepts a single client at
//! a time. Most queries (`...?`) return exactly one newline-terminated line;
//! non-query commands return nothing at all, which means a malformed set would
//! otherwise fail silently. [`ScpiClient::command`] therefore follows every set
//! with an `*OPC?` handshake: it costs a round trip, but it keeps ordering
//! deterministic and surfaces errors at the point they happen rather than
//! several commands later.
//!
//! Two properties of the server, observed on a v1.6.5 GUI, shape everything
//! here:
//!
//! - An unrecognised query is answered with silence, not an error. A misspelt
//!   command is indistinguishable from a slow one, so it costs a full timeout
//!   and misaligns every later exchange. Only command names observed to answer
//!   are sent.
//! - `VNA:TRAC:TOUCHSTONE?` answers over many lines closed by a blank one and
//!   needs [`ScpiClient::query_multiline`]; a single-line read leaves the rest
//!   queued and desynchronises the connection.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::error::{Result, VnaError};

/// How long a raw query may pause mid-response before it is judged complete.
///
/// `*LST?` sends hundreds of lines with no terminator, so quiescence is the only
/// end marker available. Generous enough that a stalled TCP segment does not
/// truncate the reply.
const RAW_IDLE: Duration = Duration::from_millis(400);

/// The SCPI server port LibreVNA-GUI listens on unless told otherwise.
///
/// From `preferences.h`: `{&SCPIServer.port, "SCPIServer.port", 19542}`.
pub const DEFAULT_SCPI_PORT: u16 = 19542;

/// A connected SCPI session.
pub struct ScpiClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    timeout: Duration,
    addr: String,
    /// Set once a response was left partly unread, which shifts every later
    /// reply onto the wrong request. Unrecoverable without a new connection.
    desynchronised: bool,
}

impl ScpiClient {
    /// Open a SCPI session to LibreVNA-GUI.
    pub async fn connect(addr: &str, timeout: Duration) -> Result<Self> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| VnaError::GuiUnavailable {
                addr: addr.to_string(),
                reason: format!("connection timed out after {timeout:?}"),
            })?
            .map_err(|e| VnaError::GuiUnavailable {
                addr: addr.to_string(),
                reason: e.to_string(),
            })?;

        // SCPI exchanges are small and latency-sensitive; batching them costs
        // far more than the extra packets save.
        let _ = stream.set_nodelay(true);

        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            timeout,
            addr: addr.to_string(),
            desynchronised: false,
        })
    }

    /// The address this session is connected to.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// How long a single exchange may take before it is abandoned.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Send a query and return its single-line response.
    pub async fn query(&mut self, command: &str) -> Result<String> {
        self.query_with_timeout(command, self.timeout).await
    }

    /// Send a query with a caller-chosen deadline.
    ///
    /// Sweeps and calibration steps can legitimately take far longer than an
    /// ordinary query, so those callers widen the budget rather than raising
    /// the timeout for every exchange.
    pub async fn query_with_timeout(&mut self, command: &str, timeout: Duration) -> Result<String> {
        self.send(command).await?;

        let mut line = String::new();
        let read = tokio::time::timeout(timeout, self.reader.read_line(&mut line))
            .await
            .map_err(|_| VnaError::Timeout(timeout, command.to_string()))?;

        match read? {
            // A clean EOF means LibreVNA-GUI closed the connection, most often
            // because another client took the single available slot.
            0 => Err(VnaError::GuiUnavailable {
                addr: self.addr.clone(),
                reason: "connection closed by LibreVNA-GUI while awaiting a response".into(),
            }),
            _ => Ok(line.trim_end_matches(['\r', '\n']).to_string()),
        }
    }

    /// Send a query whose response spans several lines, ended by a blank one.
    ///
    /// `VNA:TRAC:TOUCHSTONE?` is the only such response: a header, one row per
    /// sweep point, then an empty line. The blank line is the only end marker --
    /// the length is not known in advance, and a 4501-point 2-port export spans
    /// a dozen TCP segments, so no single read suffices.
    ///
    /// Returns the text with its internal newlines, without the terminator.
    pub async fn query_multiline(&mut self, command: &str, timeout: Duration) -> Result<String> {
        self.send(command).await?;

        let deadline = tokio::time::Instant::now() + timeout;
        let mut text = String::new();
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout_at(deadline, self.reader.read_line(&mut line))
                .await
                .map_err(|_| {
                    if text.is_empty() {
                        // Unknown requests get silence, so an empty response is
                        // far more likely a rejected request than a slow one.
                        VnaError::Parse {
                            command: command.to_string(),
                            reason: format!(
                                "the instrument sent no response within {timeout:?}. It answers \
                                 an unsupported request with silence, so this is most likely a \
                                 request it rejected rather than a slow one"
                            ),
                            raw: String::new(),
                        }
                    } else {
                        // Rows are still queued and there is no way to know how
                        // many; the session cannot be trusted again.
                        self.desynchronised = true;
                        VnaError::Timeout(timeout, command.to_string())
                    }
                })?;

            match read? {
                0 => {
                    return Err(VnaError::GuiUnavailable {
                        addr: self.addr.clone(),
                        reason: "connection closed by LibreVNA-GUI mid-response".into(),
                    });
                }
                _ => {
                    // The blank line ends the response; anything else is content.
                    if line.trim_end_matches(['\r', '\n']).is_empty() {
                        return Ok(text);
                    }
                    text.push_str(&line);
                }
            }
        }
    }

    /// Send a query whose framing is not known in advance.
    ///
    /// Most queries answer in one line, `VNA:TRAC:TOUCHSTONE?` in several closed
    /// by a blank one, and `*LST?` in several with no terminator at all. Raw
    /// SCPI can be any of the three, so this reads the first line within
    /// `timeout` and then keeps reading until a blank line or `RAW_IDLE` of
    /// quiet. Draining matters more than the text: a response left partly read
    /// desynchronises every later exchange.
    pub async fn query_raw(&mut self, command: &str, timeout: Duration) -> Result<String> {
        self.send(command).await?;

        let mut text = String::new();
        let mut first = true;
        loop {
            let budget = if first { timeout } else { RAW_IDLE };
            let mut line = String::new();
            match tokio::time::timeout(budget, self.reader.read_line(&mut line)).await {
                // Quiet for RAW_IDLE means the response is complete. On the
                // first line it means no response at all, which is how the
                // instrument refuses a command it does not accept.
                Err(_) if first => return Err(VnaError::Timeout(timeout, command.to_string())),
                Err(_) => return Ok(text),
                Ok(read) => match read? {
                    0 => {
                        return Err(VnaError::GuiUnavailable {
                            addr: self.addr.clone(),
                            reason: "connection closed by LibreVNA-GUI while awaiting a response"
                                .into(),
                        });
                    }
                    _ => {
                        if line.trim_end_matches(['\r', '\n']).is_empty() {
                            return Ok(text);
                        }
                        text.push_str(&line);
                        first = false;
                    }
                },
            }
        }
    }

    /// Send a command that produces no response, then synchronise with `*OPC?`.
    pub async fn command(&mut self, command: &str) -> Result<()> {
        self.send(command).await?;
        self.sync().await
    }

    /// Send a command and wait for completion with a caller-chosen deadline.
    pub async fn command_with_timeout(&mut self, command: &str, timeout: Duration) -> Result<()> {
        self.send(command).await?;
        let reply = self.query_with_timeout("*OPC?", timeout).await?;
        Self::check_opc(command, &reply)
    }

    /// Wait for all pending operations to complete.
    pub async fn sync(&mut self) -> Result<()> {
        let reply = self.query("*OPC?").await?;
        Self::check_opc("*OPC?", &reply)
    }

    fn check_opc(command: &str, reply: &str) -> Result<()> {
        if reply.trim() == "1" {
            Ok(())
        } else {
            Err(VnaError::Parse {
                command: command.to_string(),
                reason: "expected `1` from the *OPC? handshake".into(),
                raw: reply.to_string(),
            })
        }
    }

    /// Write one newline-terminated line to the instrument.
    /// Refuse to keep talking once the stream is known to be out of step.
    ///
    /// Continuing would return the previous exchange's answer to each new
    /// query: plausible values against the wrong question, which is far worse
    /// than an error.
    fn check_sync(&self) -> Result<()> {
        if self.desynchronised {
            return Err(VnaError::GuiUnavailable {
                addr: self.addr.clone(),
                reason: "the SCPI stream is out of step after a partly-read response; \
                         reconnect before issuing further commands"
                    .into(),
            });
        }
        Ok(())
    }

    async fn send(&mut self, command: &str) -> Result<()> {
        self.check_sync()?;
        tracing::trace!(command, "scpi tx");
        self.writer.write_all(command.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        Ok(())
    }
}
