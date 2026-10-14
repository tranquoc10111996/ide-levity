//! Transports carrying JSON-RPC messages between the client and a language server:
//! [`StdioTransport`] (child process or any byte stream with `Content-Length` framing),
//! [`WebSocketTransport`] (RFC 6455, for remote/mobile servers) and [`MockTransport`]
//! (in-process deterministic server for tests).

use crate::jsonrpc::{
    encode_lsp_frame, read_lsp_frame_bytes, JsonRpcErrorPayload, JsonRpcMessage,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
};
use crate::LspError;
use base64::Engine as _;
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};

/// Number of trailing stderr lines kept per server process for error reporting.
pub const STDERR_TAIL_LINES: usize = 60;

/// Channel pair connecting an [`crate::LspClient`] to a transport, plus the optional
/// child process backing it.
pub struct TransportChannels {
    /// Messages to send to the server.
    pub outgoing_tx: mpsc::UnboundedSender<JsonRpcMessage>,
    /// Messages received from the server. The channel closes when the server
    /// disconnects (EOF or fatal framing error).
    pub incoming_rx: Mutex<mpsc::UnboundedReceiver<JsonRpcMessage>>,
    /// The spawned server process, when the transport owns one.
    pub process: Option<ServerProcess>,
}

/// A spawned language server process. Its stderr is continuously drained into a
/// bounded ring buffer so a chatty server can never block on a full pipe.
pub struct ServerProcess {
    /// The child process (spawned with `kill_on_drop(true)`).
    pub child: Child,
    stderr_tail: Arc<parking_lot::Mutex<VecDeque<String>>>,
    program: String,
}

impl ServerProcess {
    /// Program that was launched.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Operating-system process id, if the process is still running.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Returns a handle to the last [`STDERR_TAIL_LINES`] lines written to stderr.
    pub fn stderr_tail_handle(&self) -> StderrTail {
        StderrTail(Arc::clone(&self.stderr_tail))
    }
}

/// Shared view of the most recent stderr lines of a server process.
#[derive(Clone, Default)]
pub struct StderrTail(Arc<parking_lot::Mutex<VecDeque<String>>>);

impl StderrTail {
    /// Snapshot of the buffered lines, oldest first.
    pub fn lines(&self) -> Vec<String> {
        self.0.lock().iter().cloned().collect()
    }

    /// Buffered lines joined with newlines (trimmed to the last `max` lines).
    pub fn joined(&self, max: usize) -> String {
        let guard = self.0.lock();
        let skip = guard.len().saturating_sub(max);
        guard
            .iter()
            .skip(skip)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Options for spawning a language server process.
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    /// Executable name or absolute path.
    pub program: String,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Working directory (usually the project root).
    pub cwd: Option<PathBuf>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Directories prepended to `PATH` for the child (e.g. the directory of a
    /// resolved `node`-based server so its `#!/usr/bin/env node` shebang works).
    pub extra_path: Vec<PathBuf>,
}

fn spawn_writer<W>(mut writer: W, mut out_rx: mpsc::UnboundedReceiver<JsonRpcMessage>)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let frame = match encode_lsp_frame(&msg) {
                Ok(frame) => frame,
                Err(_) => continue,
            };
            if writer.write_all(&frame).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });
}

fn spawn_reader<R>(reader: R, in_tx: mpsc::UnboundedSender<JsonRpcMessage>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf_reader = BufReader::with_capacity(64 * 1024, reader);
        while let Ok(Some(body)) = read_lsp_frame_bytes(&mut buf_reader).await {
            match JsonRpcMessage::from_slice(&body) {
                Ok(msg) => {
                    if in_tx.send(msg).is_err() {
                        break;
                    }
                }
                // A malformed body is skipped; framing is still intact.
                Err(_) => continue,
            }
        }
    });
}

/// Desktop transport that communicates over standard input/output (process stdio or duplex streams)
/// using `Content-Length` framed JSON-RPC 2.0 packets.
pub struct StdioTransport {
    channels: TransportChannels,
}

impl StdioTransport {
    /// Spawns a local Language Server process (e.g. `rust-analyzer`, `pyright-langserver`) and attaches to its stdio.
    pub fn spawn(program: &str, args: &[&str]) -> Result<Self, LspError> {
        Self::spawn_with(&SpawnOptions {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        })
    }

    /// Spawns a language server with full control over cwd, environment and `PATH`.
    pub fn spawn_with(options: &SpawnOptions) -> Result<Self, LspError> {
        let mut command = Command::new(&options.program);
        command
            .args(&options.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &options.env {
            command.env(key, value);
        }
        if !options.extra_path.is_empty() {
            let mut paths: Vec<PathBuf> = options.extra_path.clone();
            if let Some(existing) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                command.env("PATH", joined);
            }
        }

        let mut child = command.spawn().map_err(|e| LspError::Spawn {
            program: options.program.clone(),
            source: e,
        })?;

        let stdin = child.stdin.take().ok_or(LspError::TransportClosed)?;
        let stdout = child.stdout.take().ok_or(LspError::TransportClosed)?;
        let stderr = child.stderr.take().ok_or(LspError::TransportClosed)?;

        let tail: Arc<parking_lot::Mutex<VecDeque<String>>> =
            Arc::new(parking_lot::Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        let tail_writer = Arc::clone(&tail);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match lines.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                        let mut guard = tail_writer.lock();
                        if guard.len() == STDERR_TAIL_LINES {
                            guard.pop_front();
                        }
                        guard.push_back(line);
                    }
                }
            }
        });

        let mut transport = Self::from_streams(stdout, stdin);
        transport.channels.process = Some(ServerProcess {
            child,
            stderr_tail: tail,
            program: options.program.clone(),
        });
        Ok(transport)
    }

    /// Creates a `StdioTransport` from any pair of `AsyncRead` and `AsyncWrite` streams.
    pub fn from_streams<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        spawn_writer(writer, out_rx);
        spawn_reader(reader, in_tx);
        Self {
            channels: TransportChannels {
                outgoing_tx: out_tx,
                incoming_rx: Mutex::new(in_rx),
                process: None,
            },
        }
    }

    /// Consumes the transport, yielding its channels.
    pub fn into_channels(self) -> TransportChannels {
        self.channels
    }
}

/// Mobile / Remote transport that connects to a remote LSP bridge over WebSocket (RFC 6455).
///
/// Each JSON-RPC message travels as one text message (fragmented messages are
/// reassembled). Client frames are masked as the RFC requires, pings are answered
/// with pongs and the server's `Sec-WebSocket-Accept` is verified. Only plain `ws://`
/// is supported; TLS (`wss://`) must be terminated by a local proxy.
pub struct WebSocketTransport {
    channels: TransportChannels,
}

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Computes the `Sec-WebSocket-Accept` value for a given `Sec-WebSocket-Key`.
pub fn websocket_accept_key(key: &str) -> String {
    let mut sha = sha1_smol::Sha1::new();
    sha.update(key.as_bytes());
    sha.update(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(sha.digest().bytes())
}

impl WebSocketTransport {
    /// Connects to a `ws://host:port/path` endpoint and performs the HTTP Upgrade handshake.
    pub async fn connect(ws_url: &str) -> Result<Self, LspError> {
        let stripped = ws_url
            .strip_prefix("ws://")
            .ok_or_else(|| LspError::WebSocketHandshake("URL must start with ws://".into()))?;
        let (host_port, path) = match stripped.find('/') {
            Some(idx) => (&stripped[..idx], &stripped[idx..]),
            None => (stripped, "/"),
        };
        let connect_addr = if host_port.contains(':') {
            host_port.to_string()
        } else {
            format!("{host_port}:80")
        };

        let mut stream = TcpStream::connect(&connect_addr).await?;
        let _ = stream.set_nodelay(true);
        let nonce: [u8; 16] = std::array::from_fn(|_| fastrand::u8(..));
        let key = base64::engine::general_purpose::STANDARD.encode(nonce);
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host_port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;

        let (read_half, write_half) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(read_half);
        let mut status_line = String::new();
        buf_reader.read_line(&mut status_line).await?;
        let status_ok = status_line
            .split_whitespace()
            .nth(1)
            .is_some_and(|code| code == "101");
        if !status_ok {
            return Err(LspError::WebSocketHandshake(format!(
                "Expected HTTP 101 Switching Protocols, got: {}",
                status_line.trim()
            )));
        }
        let expected_accept = websocket_accept_key(&key);
        let mut accept_ok = false;
        loop {
            let mut header_line = String::new();
            let n = buf_reader.read_line(&mut header_line).await?;
            if n == 0 || header_line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header_line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("sec-websocket-accept")
                    && value.trim() == expected_accept
                {
                    accept_ok = true;
                }
            }
        }
        if !accept_ok {
            return Err(LspError::WebSocketHandshake(
                "missing or invalid Sec-WebSocket-Accept header".into(),
            ));
        }

        Ok(Self::from_stream_parts(buf_reader, write_half, true))
    }

    /// Creates a `WebSocketTransport` over an already-upgraded byte stream using RFC 6455
    /// frames. Outgoing frames are unmasked (server role); use [`Self::connect`] for the
    /// client role against real servers.
    pub fn from_framed_streams<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::from_stream_parts(reader, writer, false)
    }

    /// Creates a transport over an upgraded stream, choosing whether outgoing frames are
    /// masked (`true` for the client role, as RFC 6455 requires).
    pub fn from_stream_parts<R, W>(mut reader: R, writer: W, mask_outgoing: bool) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let writer = Arc::new(Mutex::new(writer));

        let data_writer = Arc::clone(&writer);
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    msg = out_rx.recv() => match msg {
                        Some(msg) => match serde_json::to_vec(&msg) {
                            Ok(payload) => encode_ws_frame(0x1, &payload, mask_outgoing),
                            Err(_) => continue,
                        },
                        None => break,
                    },
                    ctrl = control_rx.recv() => match ctrl {
                        Some(frame) => frame,
                        None => break,
                    },
                };
                let mut w = data_writer.lock().await;
                if w.write_all(&frame).await.is_err() || w.flush().await.is_err() {
                    break;
                }
            }
            let mut w = data_writer.lock().await;
            let _ = w.write_all(&encode_ws_frame(0x8, &[], mask_outgoing)).await;
            let _ = w.shutdown().await;
        });

        tokio::spawn(async move {
            let mut message: Vec<u8> = Vec::new();
            loop {
                let frame = match read_ws_raw_frame(&mut reader).await {
                    Ok(Some(frame)) => frame,
                    Ok(None) | Err(_) => break,
                };
                match frame.opcode {
                    0x0..=0x2 => {
                        message.extend_from_slice(&frame.payload);
                        if message.len() > crate::jsonrpc::MAX_FRAME_BYTES {
                            break;
                        }
                        if frame.fin {
                            if let Ok(msg) = JsonRpcMessage::from_slice(&message) {
                                if in_tx.send(msg).is_err() {
                                    break;
                                }
                            }
                            message.clear();
                        }
                    }
                    0x8 => break,
                    0x9 => {
                        let pong = encode_ws_frame(0xA, &frame.payload, mask_outgoing);
                        if control_tx.send(pong).is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        Self {
            channels: TransportChannels {
                outgoing_tx: out_tx,
                incoming_rx: Mutex::new(in_rx),
                process: None,
            },
        }
    }

    /// Consumes the transport, yielding its channels.
    pub fn into_channels(self) -> TransportChannels {
        self.channels
    }
}

/// Encodes a WebSocket frame with FIN set, the given opcode and optional client masking.
pub fn encode_ws_frame(opcode: u8, payload: &[u8], masked: bool) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | (opcode & 0x0F));
    let mask_bit = if masked { 0x80 } else { 0x00 };
    let len = payload.len();
    if len <= 125 {
        frame.push(mask_bit | len as u8);
    } else if len <= 65535 {
        frame.push(mask_bit | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(mask_bit | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    if masked {
        let key: [u8; 4] = std::array::from_fn(|_| fastrand::u8(..));
        frame.extend_from_slice(&key);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
    } else {
        frame.extend_from_slice(payload);
    }
    frame
}

/// Encodes an unmasked WebSocket text frame (FIN=1, opcode=1), i.e. the server role.
pub fn encode_ws_text_frame(payload: &[u8]) -> Vec<u8> {
    encode_ws_frame(0x1, payload, false)
}

/// A decoded WebSocket frame.
#[derive(Debug, Clone)]
pub struct WsFrame {
    /// Final fragment flag.
    pub fin: bool,
    /// Frame opcode (0 continuation, 1 text, 2 binary, 8 close, 9 ping, 10 pong).
    pub opcode: u8,
    /// Unmasked payload.
    pub payload: Vec<u8>,
}

/// Reads one raw WebSocket frame (any opcode), unmasking it if needed.
/// Returns `Ok(None)` on EOF.
pub async fn read_ws_raw_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<WsFrame>, LspError> {
    let mut header = [0u8; 2];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(LspError::Io(e)),
    }
    let fin = (header[0] & 0x80) != 0;
    let opcode = header[0] & 0x0F;
    let masked = (header[1] & 0x80) != 0;
    let mut len = (header[1] & 0x7F) as u64;
    if len == 126 {
        let mut ext = [0u8; 2];
        reader.read_exact(&mut ext).await?;
        len = u16::from_be_bytes(ext) as u64;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        reader.read_exact(&mut ext).await?;
        len = u64::from_be_bytes(ext);
    }
    if len > crate::jsonrpc::MAX_FRAME_BYTES as u64 {
        return Err(LspError::Protocol(format!(
            "WebSocket frame of {len} bytes exceeds limit"
        )));
    }
    let mut mask_key = [0u8; 4];
    if masked {
        reader.read_exact(&mut mask_key).await?;
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    if masked {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask_key[i % 4];
        }
    }
    Ok(Some(WsFrame {
        fin,
        opcode,
        payload,
    }))
}

/// Reads a single WebSocket data frame payload. Returns `Ok(None)` on EOF or a close frame.
pub async fn read_ws_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, LspError> {
    loop {
        match read_ws_raw_frame(reader).await? {
            None => return Ok(None),
            Some(frame) if frame.opcode == 0x8 => return Ok(None),
            Some(frame) if frame.opcode >= 0x8 => continue,
            Some(frame) => return Ok(Some(frame.payload)),
        }
    }
}

/// Mock transport for deterministic testing of all LSP requests and notifications.
pub struct MockTransport {
    channels: TransportChannels,
}

impl MockTransport {
    /// Creates a `MockTransport` paired with a server-side handler function and a notification sender.
    /// The third value records every notification the client sent to the mock server.
    #[allow(clippy::type_complexity)]
    pub fn new<F>(
        mut handler: F,
    ) -> (
        Self,
        mpsc::UnboundedSender<JsonRpcNotification>,
        Arc<RwLock<Vec<JsonRpcNotification>>>,
    )
    where
        F: FnMut(&JsonRpcRequest) -> Result<Value, JsonRpcErrorPayload> + Send + 'static,
    {
        let (client_out_tx, mut server_in_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (server_out_tx, client_in_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<JsonRpcNotification>();

        let recorded_notifications = Arc::new(RwLock::new(Vec::new()));
        let recorded_clone = Arc::clone(&recorded_notifications);
        let server_out_tx_clone = server_out_tx.clone();

        // Forward server-originated notifications (such as publishDiagnostics)
        tokio::spawn(async move {
            while let Some(notif) = notify_rx.recv().await {
                if server_out_tx_clone
                    .send(JsonRpcMessage::Notification(notif))
                    .is_err()
                {
                    break;
                }
            }
        });

        // Process incoming client requests & notifications
        tokio::spawn(async move {
            while let Some(msg) = server_in_rx.recv().await {
                match msg {
                    JsonRpcMessage::Request(req) => {
                        let response = match handler(&req) {
                            Ok(val) => JsonRpcResponse::success(req.id.clone(), val),
                            Err(err) => JsonRpcResponse {
                                jsonrpc: "2.0".to_string(),
                                id: Some(req.id.clone()),
                                result: None,
                                error: Some(err),
                            },
                        };
                        let _ = server_out_tx.send(JsonRpcMessage::Response(response));
                    }
                    JsonRpcMessage::Notification(notif) => {
                        recorded_clone.write().push(notif);
                    }
                    JsonRpcMessage::Response(_) => {}
                }
            }
        });

        (
            Self {
                channels: TransportChannels {
                    outgoing_tx: client_out_tx,
                    incoming_rx: Mutex::new(client_in_rx),
                    process: None,
                },
            },
            notify_tx,
            recorded_notifications,
        )
    }

    /// Consumes the transport, yielding its channels.
    pub fn into_channels(self) -> TransportChannels {
        self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_rfc_example() {
        assert_eq!(
            websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[tokio::test]
    async fn masked_frames_roundtrip_and_large_payloads() {
        let payload: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let frame = encode_ws_frame(0x1, &payload, true);
        assert_eq!(frame[1] & 0x80, 0x80, "mask bit must be set");
        let mut reader = &frame[..];
        let decoded = read_ws_raw_frame(&mut reader).await.unwrap().unwrap();
        assert!(decoded.fin);
        assert_eq!(decoded.opcode, 1);
        assert_eq!(decoded.payload, payload);
    }

    #[tokio::test]
    async fn websocket_reassembles_fragments_and_answers_pings() {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (mut server_read, mut server_write) = tokio::io::split(server_side);
        let transport = WebSocketTransport::from_stream_parts(client_read, client_write, true);
        let channels = transport.into_channels();

        // Ping first, then a JSON message split into two fragments.
        let body = br#"{"jsonrpc":"2.0","method":"hello","params":{"x":1}}"#;
        let (a, b) = body.split_at(10);
        let mut wire = encode_ws_frame(0x9, b"png", false);
        let mut first = vec![0x01u8, a.len() as u8];
        first.extend_from_slice(a);
        let mut second = vec![0x80u8, b.len() as u8];
        second.extend_from_slice(b);
        wire.extend(first);
        wire.extend(second);
        server_write.write_all(&wire).await.unwrap();

        let msg = channels.incoming_rx.lock().await.recv().await.unwrap();
        assert!(matches!(msg, JsonRpcMessage::Notification(n) if n.method == "hello"));

        let pong = read_ws_raw_frame(&mut server_read).await.unwrap().unwrap();
        assert_eq!(pong.opcode, 0xA);
        assert_eq!(pong.payload, b"png");
    }

    #[tokio::test]
    async fn spawn_reports_missing_program() {
        let err = StdioTransport::spawn("definitely-not-a-real-language-server-xyz", &[])
            .err()
            .expect("spawn must fail");
        assert!(matches!(err, LspError::Spawn { .. }), "{err}");
    }

    #[tokio::test]
    async fn stderr_is_drained_into_tail() {
        // Writes far more than a pipe buffer to stderr, then echoes stdin to stdout.
        let transport = StdioTransport::spawn_with(&SpawnOptions {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "i=0; while [ $i -lt 3000 ]; do echo \"noise line $i ................................\" >&2; i=$((i+1)); done; echo done-writing >&2; exec cat".into(),
            ],
            ..Default::default()
        })
        .unwrap();
        let channels = transport.into_channels();
        let tail = channels.process.as_ref().unwrap().stderr_tail_handle();
        channels
            .outgoing_tx
            .send(JsonRpcMessage::Notification(JsonRpcNotification::new(
                "echo",
                None,
            )))
            .unwrap();
        let echoed = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            async { channels.incoming_rx.lock().await.recv().await },
        )
        .await
        .expect("cat must echo once stderr is drained")
        .unwrap();
        assert!(matches!(echoed, JsonRpcMessage::Notification(n) if n.method == "echo"));
        let lines = tail.lines();
        assert!(lines.len() <= STDERR_TAIL_LINES);
        assert_eq!(lines.last().map(String::as_str), Some("done-writing"));
    }
}
