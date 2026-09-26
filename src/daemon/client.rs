//! Client side of the daemon/worker protocol: connection helper, daemon autostart, and
//! the attach loop's frame reader (design §5.3, §5.5).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf};

use crate::daemon::paths::daemon_socket_path;
use crate::daemon::protocol::{Frame, PROTO_VERSION, Request, RequestBody, Response, ResponseData};

/// A line-framed protocol connection (daemon or worker side).
pub struct Conn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
    /// "daemon" | "worker" — from the hello response.
    pub role: String,
}

impl Conn {
    /// Connect + handshake. Errors carry the next actionable step (A1 discipline). The
    /// handshake is bounded (H7) so a socket that accepts but never answers hello cannot
    /// hang `attach`/`daemon status`/`doctor` indefinitely.
    pub async fn connect(socket: &Path) -> Result<Conn> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connect {}", socket.display()))?;
        let (read_half, writer) = stream.into_split();
        let mut conn = Conn {
            reader: BufReader::new(read_half),
            writer,
            next_id: 0,
            role: String::new(),
        };
        let hello = conn.request(RequestBody::Hello {
            proto: PROTO_VERSION,
            features: vec![],
            client: Some(format!("agentpit/{}", env!("CARGO_PKG_VERSION"))),
        });
        let data = match tokio::time::timeout(Duration::from_secs(5), hello).await {
            Ok(res) => res?,
            Err(_) => {
                return Err(anyhow!(
                    "{} accepted the connection but did not answer within 5s",
                    socket.display()
                ));
            }
        };
        match data {
            ResponseData::Hello { role, .. } => conn.role = role,
            other => return Err(anyhow!("unexpected hello response: {other:?}")),
        }
        Ok(conn)
    }

    /// One request/response. Event frames arriving in between are DROPPED — use
    /// [`Conn::recv_frame`] loops (attach clients) when events matter.
    pub async fn request(&mut self, body: RequestBody) -> Result<ResponseData> {
        let resp = self.request_raw(body).await?;
        if resp.ok {
            Ok(resp.data.unwrap_or(ResponseData::Unit))
        } else {
            Err(anyhow!(
                resp.error.unwrap_or_else(|| "request failed".into())
            ))
        }
    }

    /// [`Conn::request`] bounded by `timeout`, for short control verbs (status, list, stop)
    /// where a wedged peer must not hang the caller. Never use it for `send`/`repl_cell`:
    /// those legitimately wait for a whole turn. On timeout the request is abandoned; its
    /// late response (or a frame torn by the cancelled read) is skipped by the next
    /// request's id match, so the connection stays usable.
    pub async fn request_with_timeout(
        &mut self,
        body: RequestBody,
        timeout: Duration,
    ) -> Result<ResponseData> {
        let res = tokio::time::timeout(timeout, self.request(body)).await;
        match res {
            Ok(res) => res,
            Err(_) => Err(anyhow!(
                "the {} did not answer within {timeout:?}. It may be wedged: check \
                 `agentpit daemon status`, or run `agentpit daemon stop` and retry.",
                if self.role.is_empty() {
                    "peer"
                } else {
                    &self.role
                }
            )),
        }
    }

    /// Like [`Conn::request`] but returns the raw response (callers branching on errors).
    pub async fn request_raw(&mut self, body: RequestBody) -> Result<Response> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&Request { id, body }).await?;
        loop {
            match self.recv_frame().await? {
                Frame::Response(resp) if resp.id == id => return Ok(resp),
                Frame::Response(_) | Frame::Event(_) => continue,
            }
        }
    }

    /// Fire a request WITHOUT waiting for its response (attach loops interleave manually).
    pub async fn send_request(&mut self, body: RequestBody) -> Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&Request { id, body }).await?;
        Ok(id)
    }

    async fn send(&mut self, req: &Request) -> Result<()> {
        let line = serde_json::to_string(req)?;
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        Ok(())
    }

    /// Next frame (response OR event). `Err` only on EOF (connection over). A single
    /// unparsable line is skipped rather than killing the whole connection (L7): one
    /// stray non-JSON line must not drop an attach mid-stream.
    pub async fn recv_frame(&mut self) -> Result<Frame> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                return Err(anyhow!("connection closed"));
            }
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Frame>(&line) {
                Ok(frame) => return Ok(frame),
                Err(_) => continue, // skip a torn/foreign line, keep the connection alive
            }
        }
    }
}

/// Connect to the daemon, autostarting it when absent (design §5.5: live-probe, never a
/// pidfile). `autostart=false` = probe only.
pub async fn connect_daemon(autostart: bool) -> Result<Conn> {
    let socket = daemon_socket_path();
    if let Ok(conn) = Conn::connect(&socket).await {
        return Ok(conn);
    }
    if !autostart {
        return Err(anyhow!(
            "no daemon is running. Start one with `agentpit daemon start`."
        ));
    }
    spawn_daemon()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(conn) = Conn::connect(&socket).await {
            return Ok(conn);
        }
        if std::time::Instant::now() > deadline {
            return Err(anyhow!(
                "daemon did not come up within 5s. Try `agentpit daemon start` in a terminal \
                 to see its startup error."
            ));
        }
    }
}

/// Spawn `agentpit daemon run` detached (own process group, no stdio).
pub fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("resolve agentpit binary path")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("spawn daemon")?;
    Ok(())
}

/// Ensure a worker for `session` via the daemon and connect to it.
/// Returns (resolved_session_id, worker connection).
pub async fn open_session(session: &str, autostart: bool) -> Result<(String, Conn)> {
    let mut daemon = connect_daemon(autostart).await?;
    let data = daemon
        .request(RequestBody::Ensure {
            session: session.to_string(),
        })
        .await?;
    let (session_id, socket) = match data {
        ResponseData::Session { session_id, socket } => (session_id, socket),
        other => return Err(anyhow!("unexpected ensure response: {other:?}")),
    };
    let worker = Conn::connect(&PathBuf::from(&socket))
        .await
        .with_context(|| {
            format!(
                "worker socket {socket} did not answer — retry, the daemon respawns it on demand"
            )
        })?;
    Ok((session_id, worker))
}

/// Create a fresh session via the daemon and connect to its worker.
pub async fn create_session(cwd: &Path, autostart: bool) -> Result<(String, Conn)> {
    let mut daemon = connect_daemon(autostart).await?;
    let data = daemon
        .request(RequestBody::Create {
            cwd: cwd.display().to_string(),
        })
        .await?;
    let (session_id, socket) = match data {
        ResponseData::Session { session_id, socket } => (session_id, socket),
        other => return Err(anyhow!("unexpected create response: {other:?}")),
    };
    let worker = Conn::connect(&PathBuf::from(socket)).await?;
    Ok((session_id, worker))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write_response(w: &mut OwnedWriteHalf, resp: &Response) {
        let line = serde_json::to_string(resp).unwrap();
        w.write_all(line.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
    }

    /// A peer that never answers costs the caller exactly `timeout`, the error names the
    /// limit and the next step, and the abandoned request's late response is not mistaken
    /// for the next request's answer.
    #[tokio::test]
    async fn request_with_timeout_names_the_limit_and_leaves_the_connection_usable() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (read_half, writer) = ours.into_split();
        let mut conn = Conn {
            reader: BufReader::new(read_half),
            writer,
            next_id: 0,
            role: "daemon".into(),
        };
        let (peer_r, mut peer_w) = theirs.into_split();
        let mut peer_r = BufReader::new(peer_r);

        let err = conn
            .request_with_timeout(RequestBody::Status, Duration::from_millis(50))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("daemon did not answer within 50ms"), "{msg}");
        assert!(msg.contains("agentpit daemon stop"), "{msg}");

        // The peer wakes up: it answers the abandoned request (id 1) late, then the next.
        let peer = tokio::spawn(async move {
            let mut line = String::new();
            for _ in 0..2 {
                line.clear();
                peer_r.read_line(&mut line).await.unwrap();
            }
            let stale = ResponseData::Lines {
                lines: vec!["stale".into()],
            };
            write_response(&mut peer_w, &Response::ok(1, stale)).await;
            write_response(&mut peer_w, &Response::ok(2, ResponseData::Unit)).await;
            peer_w
        });
        let data = conn
            .request_with_timeout(RequestBody::Status, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(data, ResponseData::Unit);
        drop(peer.await.unwrap());
    }
}
