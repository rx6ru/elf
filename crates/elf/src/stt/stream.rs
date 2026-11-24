//! Pass 1: the live Deepgram WebSocket.
//!
//! One thread per stream. It writes audio from a channel and reads results in the same loop,
//! so the socket has a short read timeout.

use std::collections::HashMap;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Connector, Message, WebSocket};

use super::{Failure, Message as Incoming, Params, parse_error_body, parse_message};
use crate::config::ApiKey;
use crate::machine::{Event, SessionId, Stream};

const HOST: &str = "api.deepgram.com";
const KEEP_ALIVE: Duration = Duration::from_secs(8);
/// After `CloseStream` the server closes by itself; give up waiting after this.
const CLOSE_WAIT: Duration = Duration::from_secs(3);
const READ_SLICE: Duration = Duration::from_millis(10);
const MAX_MESSAGE: usize = 32 * 1024;

#[derive(Debug, PartialEq)]
pub enum StreamEvent {
    Open,
    Transcript { text: String, is_final: bool },
    /// The stream broke or never opened.
    Failed(Failure),
    /// The server closed after we finished.
    Closed,
}

enum Cmd {
    Audio(Vec<u8>),
    /// `Finalize`, then `CloseStream`.
    Finish,
    Abort,
}

pub struct DgStream {
    events: Sender<Event>,
    workers: Mutex<HashMap<SessionId, Sender<Cmd>>>,
}

impl DgStream {
    pub fn new(events: Sender<Event>) -> Self {
        Self { events, workers: Mutex::new(HashMap::new()) }
    }

    fn send(&self, id: SessionId, cmd: Cmd) {
        if let Some(tx) = self.workers.lock().unwrap().get(&id) {
            let _ = tx.send(cmd);
        }
    }
}

impl Stream for DgStream {
    fn open(&self, id: SessionId, key: &ApiKey, params: &Params, replay: Vec<u8>) {
        let (tx, rx) = mpsc::channel();
        if !replay.is_empty() {
            let _ = tx.send(Cmd::Audio(replay));
        }
        self.workers.lock().unwrap().insert(id, tx);
        let url = format!("wss://{HOST}/v1/listen?{}", params.query(true));
        let (key, events) = (key.clone(), self.events.clone());
        std::thread::spawn(move || {
            let emit = |e| {
                let _ = events.send(Event::Stream(id, e));
            };
            match connect(&url, &key) {
                Ok(ws) => {
                    emit(StreamEvent::Open);
                    run(ws, &rx, &emit);
                }
                Err(f) => emit(StreamEvent::Failed(f)),
            }
        });
    }

    fn audio(&self, id: SessionId, pcm: Vec<u8>) {
        self.send(id, Cmd::Audio(pcm));
    }

    fn finish(&self, id: SessionId) {
        self.send(id, Cmd::Finish);
        self.workers.lock().unwrap().remove(&id);
    }

    fn abort(&self, id: SessionId) {
        self.send(id, Cmd::Abort);
        self.workers.lock().unwrap().remove(&id);
    }
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("ring supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

fn connect(url: &str, key: &ApiKey) -> Result<Socket, Failure> {
    let other = |what: &str, e: &dyn std::fmt::Display| Failure::Other(format!("{what}: {e}"));
    let mut request = url.into_client_request().map_err(|e| other("request", &e))?;
    let auth = HeaderValue::from_str(&format!("Token {}", key.expose())).map_err(|e| other("key", &e))?;
    request.headers_mut().insert("Authorization", auth);

    let addr = (HOST, 443)
        .to_socket_addrs()
        .map_err(|e| other("dns", &e))?
        .next()
        .ok_or_else(|| Failure::Other("dns: no address".into()))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| other("connect", &e))?;
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(8)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(8)));

    let (ws, _) = tungstenite::client_tls_with_config(request, tcp, None, Some(Connector::Rustls(tls_config())))
        .map_err(|e| match e {
            tungstenite::HandshakeError::Failure(tungstenite::Error::Http(resp)) => {
                match resp.status().as_u16() {
                    401 | 403 => Failure::Auth,
                    402 => Failure::Credit,
                    429 => Failure::RateLimited,
                    code => {
                        let body = resp.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default();
                        Failure::Other(format!("http {code} {}", parse_error_body(&body)))
                    }
                }
            }
            other_err => Failure::Other(format!("handshake: {other_err}")),
        })?;
    set_read_timeout(&ws, READ_SLICE);
    Ok(ws)
}

fn set_read_timeout(ws: &Socket, d: Duration) {
    let tcp = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => s.get_ref(),
        _ => return,
    };
    let _ = tcp.set_read_timeout(Some(d));
}

fn run(mut ws: Socket, rx: &Receiver<Cmd>, emit: &dyn Fn(StreamEvent)) {
    let mut last_sent = Instant::now();
    let mut finishing: Option<Instant> = None;
    let fail = |e: tungstenite::Error| StreamEvent::Failed(Failure::Other(e.to_string()));
    loop {
        // Everything queued since the last pass goes out in messages of at most 32 KB.
        let mut pending: Vec<u8> = Vec::new();
        let mut finish = false;
        loop {
            match rx.try_recv() {
                Ok(Cmd::Audio(b)) => pending.extend(b),
                Ok(Cmd::Finish) => finish = true,
                // `finish` drops the sender; the stream still has to flush and close.
                Err(TryRecvError::Disconnected) if finish || finishing.is_some() => break,
                Ok(Cmd::Abort) | Err(TryRecvError::Disconnected) => {
                    let _ = ws.close(None);
                    return;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        let mut sends: Vec<Message> = pending.chunks(MAX_MESSAGE).map(|c| Message::binary(c.to_vec())).collect();
        if finish {
            sends.push(Message::text(r#"{"type":"Finalize"}"#));
            sends.push(Message::text(r#"{"type":"CloseStream"}"#));
            finishing = Some(Instant::now());
        } else if sends.is_empty() && last_sent.elapsed() >= KEEP_ALIVE {
            sends.push(Message::text(r#"{"type":"KeepAlive"}"#));
        }
        for m in sends {
            if let Err(e) = ws.send(m) {
                emit(fail(e));
                return;
            }
            last_sent = Instant::now();
        }

        match ws.read() {
            Ok(Message::Text(t)) => match parse_message(&t) {
                Incoming::Transcript { text, is_final } => emit(StreamEvent::Transcript { text, is_final }),
                Incoming::Error(e) => {
                    emit(StreamEvent::Failed(Failure::Other(e)));
                    return;
                }
                _ => {}
            },
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                emit(if finishing.is_some() { StreamEvent::Closed } else { StreamEvent::Failed(Failure::Other("closed by server".into())) });
                return;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => {
                emit(if finishing.is_some() { StreamEvent::Closed } else { fail(e) });
                return;
            }
        }
        if finishing.is_some_and(|t| t.elapsed() > CLOSE_WAIT) {
            emit(StreamEvent::Closed);
            return;
        }
    }
}
