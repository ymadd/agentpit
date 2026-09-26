//! One NDJSON protocol connection (daemon or loop runner), the dashboard's side of
//! `agentpit_events::wire`.
//!
//! [`Conn::recv`] keeps partial lines in the connection, so it is safe to race against a
//! timer in `tokio::select!`: a cancelled read loses nothing.

use std::path::Path;
use std::time::Duration;

use agentpit_events::wire::{
    Frame, Request, RequestBody, Response, ResponseData, FEATURE_LOOPS, PROTO_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use super::BridgeError;

/// Connect + hello. A socket that accepts but never answers must not hang the bridge.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Every request/response the bridge makes (design §11: each request carries a timeout).
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
/// A peer that sends a line longer than this without a newline is not speaking the protocol.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub struct Conn {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    /// Bytes read but not yet returned as frames (at most one partial line at the end).
    buf: Vec<u8>,
    /// How far `buf` is known to hold no newline.
    scanned: usize,
    next_id: u64,
    /// `"daemon"` or `"loop"`, from the hello.
    pub role: String,
    pub features: Vec<String>,
}

impl Conn {
    /// Connect and say hello, offering `loops/1`.
    pub async fn connect(socket: &Path, client: &str) -> Result<Conn, BridgeError> {
        let open = async {
            let stream = UnixStream::connect(socket).await.map_err(|e| {
                BridgeError::new(
                    "unreachable",
                    format!("could not connect to {}: {e}", socket.display()),
                )
            })?;
            let (reader, writer) = stream.into_split();
            let mut conn = Conn {
                reader,
                writer,
                buf: Vec::new(),
                scanned: 0,
                next_id: 0,
                role: String::new(),
                features: Vec::new(),
            };
            let hello = conn
                .call(RequestBody::Hello {
                    proto: PROTO_VERSION,
                    features: vec![FEATURE_LOOPS.into()],
                    client: Some(client.to_string()),
                })
                .await?;
            match hello {
                ResponseData::Hello { role, features, .. } => {
                    conn.role = role;
                    conn.features = features;
                    Ok(conn)
                }
                other => Err(BridgeError::protocol(format!(
                    "unexpected hello answer: {other:?}"
                ))),
            }
        };
        match tokio::time::timeout(HELLO_TIMEOUT, open).await {
            Ok(res) => res,
            Err(_) => Err(BridgeError::new(
                "timeout",
                format!(
                    "{} accepted the connection but did not answer within {}s",
                    socket.display(),
                    HELLO_TIMEOUT.as_secs()
                ),
            )),
        }
    }

    pub fn has_loops(&self) -> bool {
        self.features.iter().any(|f| f == FEATURE_LOOPS)
    }

    /// One request, answered within [`CONTROL_TIMEOUT`]. Event frames in between are
    /// dropped: use [`Conn::send`] + [`Conn::recv`] where events matter.
    pub async fn request(&mut self, body: RequestBody) -> Result<ResponseData, BridgeError> {
        match tokio::time::timeout(CONTROL_TIMEOUT, self.call(body)).await {
            Ok(res) => res,
            Err(_) => Err(BridgeError::new(
                "timeout",
                format!(
                    "the {} did not answer within {}s",
                    if self.role.is_empty() {
                        "peer"
                    } else {
                        &self.role
                    },
                    CONTROL_TIMEOUT.as_secs()
                ),
            )),
        }
    }

    async fn call(&mut self, body: RequestBody) -> Result<ResponseData, BridgeError> {
        let id = self.send(body).await?;
        loop {
            match self.recv().await? {
                Frame::Response(resp) if resp.id == id => return response_data(resp),
                _ => continue,
            }
        }
    }

    /// Send a request without waiting; returns its id.
    pub async fn send(&mut self, body: RequestBody) -> Result<u64, BridgeError> {
        self.next_id += 1;
        let id = self.next_id;
        let mut line = serde_json::to_vec(&Request { id, body })
            .map_err(|e| BridgeError::internal(format!("encode a request: {e}")))?;
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .await
            .map_err(|e| BridgeError::closed(format!("the {} went away: {e}", self.role)))?;
        Ok(id)
    }

    /// The next frame. `Err` when the connection is over. A line that is not a frame (a
    /// newer peer's, or torn) is skipped rather than ending the connection.
    pub async fn recv(&mut self) -> Result<Frame, BridgeError> {
        loop {
            if let Some(pos) = self.buf[self.scanned..].iter().position(|b| *b == b'\n') {
                let end = self.scanned + pos;
                let parsed = serde_json::from_slice::<Frame>(&self.buf[..end]);
                self.buf.drain(..=end);
                self.scanned = 0;
                match parsed {
                    Ok(frame) => return Ok(frame),
                    Err(_) => continue,
                }
            }
            self.scanned = self.buf.len();
            if self.buf.len() > MAX_FRAME_BYTES {
                return Err(BridgeError::protocol(format!(
                    "the {} sent a line over {} bytes",
                    self.role, MAX_FRAME_BYTES
                )));
            }
            let mut chunk = [0u8; 16 * 1024];
            let n =
                self.reader.read(&mut chunk).await.map_err(|e| {
                    BridgeError::closed(format!("the {} went away: {e}", self.role))
                })?;
            if n == 0 {
                return Err(BridgeError::closed(format!(
                    "the {} closed the connection",
                    if self.role.is_empty() {
                        "peer"
                    } else {
                        &self.role
                    }
                )));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// A response as a result, keeping the server's code, message and details.
pub fn response_data(resp: Response) -> Result<ResponseData, BridgeError> {
    if resp.ok {
        return Ok(resp.data.unwrap_or(ResponseData::Unit));
    }
    Err(BridgeError {
        code: resp.code.unwrap_or_else(|| "failed".into()),
        message: resp.error.unwrap_or_else(|| "request failed".into()),
        details: resp.details,
    })
}
