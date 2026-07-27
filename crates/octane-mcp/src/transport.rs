//! Transports.
//!
//! Two, both behind the same trait so [`crate::McpClient`] cannot tell them
//! apart: **stdio**, which spawns the server as a child process and exchanges
//! newline-delimited JSON over its pipes, and **streamable HTTP**, which POSTs
//! each message to one endpoint and reads the reply back as either a single JSON
//! object or an SSE stream.
//!
//! Two pieces of the HTTP transport are deliberately absent. There is no standing
//! `GET` for server-initiated messages — nothing in this client consumes an
//! unsolicited request, so opening a second connection would only hold a socket
//! open. And a `401` is an error, not the start of OAuth Dynamic Client
//! Registration (RFC 7591): registering this client with an arbitrary remote
//! authorization server is a decision the user should make, not a retry path.

use std::collections::BTreeMap;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, Mutex};

use crate::McpError;

#[async_trait]
pub trait Transport: Send + Sync {
    /// Send one message.
    async fn send(&self, message: &str) -> Result<(), McpError>;

    /// Tell the transport which version the handshake settled on.
    ///
    /// A no-op for stdio, which carries no version out of band — the default is
    /// here rather than on each implementor so adding a transport does not
    /// require thinking about a header only one of them has.
    async fn negotiated_version(&self, _version: &str) {}

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

/// Header the server hands out on initialize and expects back on every request.
const SESSION_HEADER: &str = "Mcp-Session-Id";
const PROTOCOL_HEADER: &str = "MCP-Protocol-Version";

/// A remote server reached over streamable HTTP.
///
/// The shape does not match the trait's: one POST carries a message *and* brings
/// its reply back, while `recv` is supposed to be a separate act. So `send` does
/// the round trip and queues whatever came back, and `recv` drains that queue.
/// The alternative — a background task per connection — buys nothing, because a
/// POST's reply belongs to that POST and to no other.
pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    /// Handed out on initialize, echoed from then on. Absent is legal: a
    /// stateless server keeps no session and sends no id.
    session: Mutex<Option<HeaderValue>>,
    /// The version the handshake settled on, once it has.
    ///
    /// `None` until then, which is also what the spec wants: the header is only
    /// required on requests *after* initialization, and a server that speaks an
    /// older version MUST reject a request naming one it does not support.
    /// Sending this client's maximum unconditionally broke every server except
    /// the newest — the exact servers `SUPPORTED_VERSIONS` exists to reach.
    negotiated: Mutex<Option<HeaderValue>>,
    decoded: mpsc::UnboundedSender<String>,
    pending: Mutex<mpsc::UnboundedReceiver<String>>,
}

impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Headers are excluded on purpose: this one is usually a bearer token.
        f.debug_struct("HttpTransport").field("url", &self.url).finish_non_exhaustive()
    }
}

impl HttpTransport {
    /// Prepare a connection to `url`. Nothing is sent until the first message.
    ///
    /// `headers` are applied to every request, which is how auth is carried.
    pub fn connect(url: &str, headers: &BTreeMap<String, String>) -> Result<Self, McpError> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let header = HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| McpError::Transport(format!("header {name}: {error}")))?;
            let mut value = HeaderValue::from_str(value)
                .map_err(|error| McpError::Transport(format!("header {name}: {error}")))?;
            // Keeps the token out of any header dump reqwest or a proxy logs.
            value.set_sensitive(true);
            map.insert(header, value);
        }

        let client = reqwest::Client::builder()
            // Matches the client's own per-request budget. Without it a server
            // that accepts the POST and never answers hangs `send`, which is
            // outside the timeout `McpClient::call` wraps around `recv`.
            .timeout(crate::client::DEFAULT_TIMEOUT)
            // Not followed. `set_sensitive` above keeps a token out of logs, but
            // it does nothing about where the token is *sent*: reqwest strips
            // only `Authorization`, `Cookie` and `Proxy-Authorization` across a
            // host change, so an `X-Api-Key` — and the session id — would ride a
            // redirect to whatever host the server named. Nothing in the MCP
            // flow needs a redirect, so the safe reading of one is a error.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| McpError::Transport(error.to_string()))?;

        let (decoded, pending) = mpsc::unbounded_channel();
        Ok(Self {
            client,
            url: url.to_string(),
            headers: map,
            session: Mutex::new(None),
            negotiated: Mutex::new(None),
            decoded,
            pending: Mutex::new(pending),
        })
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn negotiated_version(&self, version: &str) {
        // An unrepresentable version is simply not sent: the request then looks
        // like a pre-negotiation one, which a server tolerates, where a bad
        // header value would make every later request fail.
        if let Ok(value) = HeaderValue::from_str(version) {
            *self.negotiated.lock().await = Some(value);
        }
    }

    async fn send(&self, message: &str) -> Result<(), McpError> {
        let mut request = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            // Both are offered because the server picks: a lone JSON reply, or a
            // stream. Offering only one makes a conforming server refuse.
            .header(ACCEPT, "application/json, text/event-stream")
            .body(message.to_string());

        if let Some(version) = self.negotiated.lock().await.clone() {
            request = request.header(PROTOCOL_HEADER, version);
        }

        if let Some(session) = self.session.lock().await.clone() {
            request = request.header(SESSION_HEADER, session);
        }

        let response =
            request.send().await.map_err(|error| McpError::Transport(error.to_string()))?;

        // Recorded before the status check: a server may hand out the id and
        // still answer the request with an error.
        if let Some(id) = response.headers().get(SESSION_HEADER) {
            *self.session.lock().await = Some(id.clone());
        }

        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();

        if !status.is_success() {
            // A 404 against a live session means the server dropped it and the
            // spec's remedy is to initialize again. That is a reconnect, which
            // belongs to whoever owns the client, so it is reported, not retried.
            let body = response.text().await.unwrap_or_default();
            return Err(McpError::Transport(format!(
                "{} {}: {}",
                status.as_u16(),
                self.url,
                body.trim()
            )));
        }

        // 202 with no body is how a server acknowledges a notification.
        if status == reqwest::StatusCode::ACCEPTED {
            return Ok(());
        }

        // ponytail: the whole body is read before anything is queued, so a server
        // that holds its SSE stream open past the response stalls until the
        // timeout. The spec says it SHOULD close; stream it incrementally if one
        // in the wild does not.
        let body = response.text().await.map_err(|error| McpError::Transport(error.to_string()))?;

        let messages = if content_type.starts_with("text/event-stream") {
            sse_payloads(&body)
        } else {
            vec![body]
        };

        for message in messages {
            if message.trim().is_empty() {
                continue;
            }
            // Fails only once the receiver is gone, which means nobody is left to
            // read the reply anyway.
            let _ = self.decoded.send(message);
        }
        Ok(())
    }

    async fn recv(&self) -> Result<Option<String>, McpError> {
        // Never `None` while this transport is alive, since it holds the sender:
        // an empty queue means the reply has not been fetched yet, not that the
        // peer hung up. `McpClient::call` is what bounds the wait.
        Ok(self.pending.lock().await.recv().await)
    }

    async fn shutdown(&self) -> Result<(), McpError> {
        let Some(session) = self.session.lock().await.clone() else {
            // Stateless server: there is nothing on the far side to tear down.
            return Ok(());
        };

        // Best effort. A server is allowed to answer 405 because it does not
        // support explicit termination, and there is nothing to do about that.
        let _ = self
            .client
            .delete(&self.url)
            .headers(self.headers.clone())
            .header(SESSION_HEADER, session)
            .send()
            .await;
        Ok(())
    }
}

/// Pull the `data:` payloads out of a complete SSE body, one per event.
///
/// Not `octane_provider::sse`, which is the same parse: that crate sits in the
/// provider chain, and reaching across for it would give `octane-mcp` — and so
/// every consumer of it — a dependency on the model registry and four codecs to
/// borrow twelve lines. Its parser also earns its complexity from *chunks*
/// arriving mid-line, whereas this reads one finished body.
fn sse_payloads(body: &str) -> Vec<String> {
    let mut payloads = Vec::new();
    let mut data = String::new();

    // `lines` already handles CRLF, which some proxies rewrite everything into.
    for line in body.lines() {
        if line.is_empty() {
            if !data.is_empty() {
                payloads.push(std::mem::take(&mut data));
            }
            continue;
        }
        // `event:`, `id:` and comments are ignored: MCP puts one whole JSON-RPC
        // message in `data` and dispatches on the message, not the event name.
        if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
        }
    }

    // A last event whose trailing blank line never arrived is still an event.
    if !data.is_empty() {
        payloads.push(data);
    }
    payloads
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};

    use crate::client::McpClient;

    const INIT_RESULT: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"remote","version":"1"}}}"#;

    /// The header must name what the handshake settled on, not this client's
    /// maximum. A server on an older supported version MUST reject a request
    /// naming a version it does not speak — so sending our own ceiling breaks
    /// exactly the servers `SUPPORTED_VERSIONS` exists to reach.
    #[tokio::test]
    async fn the_protocol_version_sent_is_the_one_negotiated_not_our_own_maximum() {
        const OLDER: &str = "2025-06-18";
        let init = format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{OLDER}","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"old","version":"1"}}}}}}"#
        );
        let server = FakeServer::start(vec![
            json(&init),
            accepted(),
            json(r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#),
        ])
        .await;

        let transport = Arc::new(HttpTransport::connect(&server.url, &BTreeMap::new()).unwrap());
        let client = McpClient::new("old", transport);
        client.initialize().await.unwrap();
        client.list_tools().await.unwrap();

        let requests = server.requests();
        assert!(
            !requests[0].to_lowercase().contains("mcp-protocol-version"),
            "the initialize request predates negotiation, so it names no version",
        );
        // Lowercased: reqwest normalises header names on the wire.
        assert!(
            requests[1].to_lowercase().contains(&format!("mcp-protocol-version: {OLDER}")),
            "later requests must carry the negotiated version, got: {:?}",
            requests[1],
        );
        assert!(
            !requests[1].contains(crate::protocol::PROTOCOL_VERSION),
            "our own maximum must not appear once an older version was agreed",
        );
    }

    /// A server that speaks just enough HTTP: one request per connection, one
    /// canned answer, then close.
    ///
    /// A real socket rather than a mocked `reqwest`, because the property under
    /// test is that a message reaches the wire with the right headers on it and
    /// the reply comes back off it. A mock would only prove the mock was called.
    struct FakeServer {
        url: String,
        requests: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeServer {
        async fn start(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/mcp", listener.local_addr().unwrap());
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));

            let recorded = requests.clone();
            tokio::spawn(async move {
                for response in responses {
                    let Ok((mut socket, _)) = listener.accept().await else { return };
                    let request = read_request(&mut socket).await;
                    recorded.lock().unwrap().push(request);
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
            });

            Self { url, requests }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    async fn read_request(socket: &mut TcpStream) -> String {
        let mut raw = Vec::new();
        let mut chunk = [0u8; 512];

        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..read]);

            // Headers can arrive without the body; stop only once the declared
            // body is in hand, or the assertion runs against half a request.
            let text = String::from_utf8_lossy(&raw).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if body.len() >= length {
                    return text;
                }
            }
        }
        String::from_utf8_lossy(&raw).to_string()
    }

    /// `Connection: close` on every answer, so the next request opens a new
    /// connection and the single-threaded accept loop above keeps up.
    fn response(status: &str, extra: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
             Connection: close\r\n{extra}\r\n{body}",
            body.len()
        )
    }

    fn sse(extra: &str, message: &str) -> String {
        let body = format!("event: message\ndata: {message}\n\n");
        response("200 OK", extra, "text/event-stream", &body)
    }

    fn json(message: &str) -> String {
        response("200 OK", "", "application/json", message)
    }

    fn accepted() -> String {
        response("202 Accepted", "", "application/json", "")
    }

    #[tokio::test]
    async fn an_initialize_over_http_reaches_the_server_and_the_reply_comes_back() {
        let server = FakeServer::start(vec![sse("", INIT_RESULT), accepted()]).await;
        let transport = HttpTransport::connect(&server.url, &BTreeMap::new()).unwrap();
        let client = McpClient::new("remote", Arc::new(transport));

        let capabilities = client.initialize().await.unwrap();
        assert!(capabilities.tools.is_some());

        let requests = server.requests();
        assert!(requests[0].contains(r#""method":"initialize""#), "{}", requests[0]);
        // A conforming server refuses unless both answer shapes are acceptable.
        assert!(requests[0].contains("text/event-stream"), "{}", requests[0]);
        assert!(requests[1].contains("notifications/initialized"), "{}", requests[1]);
    }

    #[tokio::test]
    async fn a_session_id_is_echoed_on_every_request_after_the_one_that_issued_it() {
        let tools = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","inputSchema":{}}]}}"#;
        let server = FakeServer::start(vec![
            sse("Mcp-Session-Id: s3cr3t\r\n", INIT_RESULT),
            accepted(),
            // Plain JSON here, since a server may answer either way and both
            // have to reach the client.
            json(tools),
        ])
        .await;

        let transport = HttpTransport::connect(&server.url, &BTreeMap::new()).unwrap();
        let client = McpClient::new("remote", Arc::new(transport));
        client.initialize().await.unwrap();

        let listed = client.list_tools().await.unwrap();
        assert_eq!(listed[0].name, "search");

        let requests = server.requests();
        // The negative control: there was no session to echo on the first
        // request, so an id appearing there would mean it came from nowhere.
        assert!(!requests[0].contains("s3cr3t"), "{}", requests[0]);
        for request in &requests[1..] {
            // Lower-cased: that is how the name goes out on the wire, and header
            // names are case-insensitive anyway.
            assert!(request.contains("mcp-session-id: s3cr3t"), "{request}");
        }
    }

    #[tokio::test]
    async fn a_configured_header_is_sent_on_every_request() {
        let headers =
            BTreeMap::from([("Authorization".to_string(), "Bearer t0ken".to_string())]);
        let server = FakeServer::start(vec![sse("", INIT_RESULT), accepted()]).await;

        let transport = HttpTransport::connect(&server.url, &headers).unwrap();
        McpClient::new("remote", Arc::new(transport)).initialize().await.unwrap();

        for request in server.requests() {
            assert!(request.contains("authorization: Bearer t0ken"), "{request}");
        }
    }

    #[tokio::test]
    async fn an_http_error_is_reported_rather_than_read_as_a_message() {
        let server = FakeServer::start(vec![response(
            "401 Unauthorized",
            "",
            "application/json",
            "token expired",
        )])
        .await;

        let transport = HttpTransport::connect(&server.url, &BTreeMap::new()).unwrap();
        let error = transport.send("{}").await.unwrap_err();
        assert!(error.to_string().contains("401"), "{error}");
    }

    #[test]
    fn several_messages_in_one_sse_stream_all_come_out() {
        // A server may batch a notification ahead of the response it belongs to.
        let body = ": keep-alive\n\nevent: message\ndata: {\"a\":1}\n\ndata: {\"b\":2}\n\n";
        assert_eq!(sse_payloads(body), vec![r#"{"a":1}"#, r#"{"b":2}"#]);
    }

    #[test]
    fn a_data_field_split_across_lines_is_rejoined() {
        assert_eq!(sse_payloads("data: {\ndata: }\n\n"), vec!["{\n}"]);
    }
}
