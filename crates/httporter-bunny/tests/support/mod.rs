//! Helpers shared by the integration tests: a small HTTP/1.1 server that counts the connections
//! requests arrive on, and a way to drive a paused clock while real sockets do their work.

// Each test file uses only some of these.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use wiremock::MockServer;

/// While it lives, a paused clock moves only when the test calls `tokio::time::advance`.
///
/// Requests go over real sockets, which take real time. Left alone, a paused clock leaps to the
/// next timer whenever the runtime has nothing to do but wait for a socket, so a request would
/// "time out" before the server could answer. Tokio holds that leaping back while a blocking task
/// runs; this is one that waits for the guard to be dropped.
pub struct ClockHold(std::sync::mpsc::Sender<()>);

pub fn hold_clock() -> ClockHold {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        // Returns once the guard (the only sender) is dropped.
        let _ = rx.recv();
    });
    ClockHold(tx)
}

/// Lets the other tasks on the runtime catch up: above all a finished connection going back to
/// its pool, which happens in a task of its own.
pub async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

/// Waits until `server` has received `n` requests. It spins rather than sleeps, so a paused
/// clock never moves meanwhile; the wall-clock limit only stops a broken test from hanging.
pub async fn received(server: &MockServer, n: usize) {
    let give_up = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let got = server.received_requests().await.map_or(0, |r| r.len());
        if got >= n {
            return;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the server got {got} of {n} requests"
        );
        tokio::task::yield_now().await;
    }
}

/// A request's first line and headers (names in lower case).
pub struct Head {
    pub line: String,
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn content_length(&self) -> usize {
        self.header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }
}

/// One accepted connection, read as HTTP/1.1.
pub struct Conn {
    sock: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    pub fn new(sock: TcpStream) -> Self {
        Self {
            sock,
            buf: Vec::new(),
        }
    }

    /// Reads more from the socket; false once the client has hung up.
    async fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 16 * 1024];
        match self.sock.read(&mut chunk).await {
            Ok(0) | Err(_) => false,
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
        }
    }

    /// The next request's line and headers, or `None` once the client has hung up.
    pub async fn head(&mut self) -> Option<Head> {
        let end = loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if !self.fill().await {
                return None;
            }
        };
        let text = String::from_utf8_lossy(&self.buf[..end]).into_owned();
        self.buf.drain(..end);
        let mut lines = text.split("\r\n").filter(|l| !l.is_empty());
        let line = lines.next()?.to_owned();
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
            .collect();
        Some(Head { line, headers })
    }

    /// The next `n` bytes, or `None` if the client hangs up first.
    pub async fn bytes(&mut self, n: usize) -> Option<Vec<u8>> {
        while self.buf.len() < n {
            if !self.fill().await {
                return None;
            }
        }
        Some(self.buf.drain(..n).collect())
    }

    /// A whole request: its head and its `Content-Length` body.
    pub async fn request(&mut self) -> Option<(Head, Vec<u8>)> {
        let head = self.head().await?;
        let body = self.bytes(head.content_length()).await?;
        Some((head, body))
    }

    /// Sends `answer` as it is. A client that has gone away is not an error here.
    pub async fn send(&mut self, answer: &[u8]) {
        let _ = self.sock.write_all(answer).await;
        let _ = self.sock.flush().await;
    }
}

/// An answer with a `Content-Length` body.
pub fn answer(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// An answer whose body comes in several chunks (`Transfer-Encoding: chunked`). A client that
/// drops it unread leaves chunks behind, and the connection is closed rather than reused.
pub fn chunked_answer(status: &str, chunks: &[&str]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\ntransfer-encoding: chunked\r\n\r\n");
    for chunk in chunks {
        out.push_str(&format!("{:x}\r\n{chunk}\r\n", chunk.len()));
    }
    out.push_str("0\r\n\r\n");
    out.into_bytes()
}

/// Answers requests, whichever connection they come on, with canned answers in order, and
/// counts the connections.
pub struct ScriptedServer {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    /// The request line of each request, with the number of the connection it came on.
    requests: Arc<Mutex<Vec<(usize, String)>>>,
}

impl ScriptedServer {
    pub async fn start(answers: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let answers = Arc::new(Mutex::new(VecDeque::from(answers)));
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn({
            let connections = Arc::clone(&connections);
            let requests = Arc::clone(&requests);
            async move {
                while let Ok((sock, _)) = listener.accept().await {
                    let id = connections.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve(
                        Conn::new(sock),
                        id,
                        Arc::clone(&answers),
                        Arc::clone(&requests),
                    ));
                }
            }
        });
        Self {
            addr,
            connections,
            requests,
        }
    }

    pub fn uri(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// How many connections were accepted.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Each request line, with the connection (numbered from 0) it came on.
    pub fn requests(&self) -> Vec<(usize, String)> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve(
    mut conn: Conn,
    id: usize,
    answers: Arc<Mutex<VecDeque<Vec<u8>>>>,
    requests: Arc<Mutex<Vec<(usize, String)>>>,
) {
    while let Some((head, _body)) = conn.request().await {
        requests.lock().unwrap().push((id, head.line));
        let next = answers.lock().unwrap().pop_front();
        let answer = next.unwrap_or_else(|| answer("500 Internal Server Error", "out of answers"));
        conn.send(&answer).await;
    }
}
