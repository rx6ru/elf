//! Pass 2: one HTTPS request with the whole recording.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use super::{Params, parse_batch, parse_error_body};
use crate::config::ApiKey;
use crate::machine::{Batch, Event, SessionId, Upload};

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum BatchError {
    /// 401 / 403.
    Auth,
    Status(u16, String),
    Timeout,
    Network(String),
    BadBody(String),
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            BatchError::Auth => write!(f, "key rejected"),
            BatchError::Status(code, msg) => write!(f, "http {code} {msg}"),
            BatchError::Timeout => write!(f, "timeout"),
            BatchError::Network(e) => write!(f, "network: {e}"),
            BatchError::BadBody(e) => write!(f, "{e}"),
        }
    }
}

/// Timeouts, 5xx, 429 and network errors can pass on a second try; a bad key or a bad request cannot.
pub fn is_retryable(e: &BatchError) -> bool {
    match e {
        BatchError::Auth | BatchError::BadBody(_) => false,
        BatchError::Status(code, _) => *code == 429 || *code >= 500,
        BatchError::Timeout | BatchError::Network(_) => true,
    }
}

/// 6 s plus 0.3 s per second of audio.
pub fn timeout_for(audio_ms: u64) -> Duration {
    Duration::from_millis(6000 + audio_ms * 3 / 10)
}

fn status_error(code: u16, body: &str) -> BatchError {
    match code {
        401 | 403 => BatchError::Auth,
        _ => BatchError::Status(code, parse_error_body(body)),
    }
}

/// How long a kept-alive connection may idle. Warmed at the start of a dictation, it should
/// still be open when the dictation stops.
const POOL_IDLE: Duration = Duration::from_secs(30);

pub struct DgBatch {
    events: Sender<Event>,
    /// One agent, so the connection opened by `warm_up` is the one pass 2 reuses.
    agent: ureq::Agent,
}

impl DgBatch {
    pub fn new(events: Sender<Event>) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_idle_age(POOL_IDLE)
            .build()
            .into();
        Self { events, agent }
    }
}

impl Batch for DgBatch {
    fn submit(&self, id: SessionId, key: &ApiKey, upload: &Upload, params: &Params, timeout: Duration) {
        let url = format!("https://api.deepgram.com/v1/listen?{}", params.query(false));
        let (key, events, agent, upload) = (key.clone(), self.events.clone(), self.agent.clone(), upload.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = request(&agent, &url, &key, &upload, timeout);
            crate::elog!("{id} pass2 http_ms={}", started.elapsed().as_millis());
            let _ = events.send(Event::Batch(id, result));
        });
    }

    /// A plain GET with no key: any answer leaves a TLS connection in the pool.
    fn warm_up(&self) {
        let agent = self.agent.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let warmed = agent.get("https://api.deepgram.com/").call().and_then(|mut r| r.body_mut().read_to_vec());
            let ms = started.elapsed().as_millis();
            if warmed.is_err() || ms > 1500 {
                crate::elog!("warm-up ok={} ms={ms}", warmed.is_ok());
            }
        });
    }
}

fn request(agent: &ureq::Agent, url: &str, key: &ApiKey, upload: &Upload, timeout: Duration) -> Result<String, BatchError> {
    let net = |e: ureq::Error| match e {
        ureq::Error::Timeout(_) => BatchError::Timeout,
        other => BatchError::Network(other.to_string()),
    };
    let mut resp = agent
        .post(url)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .header("Authorization", format!("Token {}", key.expose()))
        .header("Content-Type", upload.content_type)
        .send(&upload.bytes[..])
        .map_err(net)?;
    let code = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().map_err(net)?;
    if code != 200 {
        return Err(status_error(code, &body));
    }
    parse_batch(&body).map_err(BatchError::BadBody)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_decisions() {
        assert!(is_retryable(&BatchError::Timeout));
        assert!(is_retryable(&BatchError::Network("offline".into())));
        assert!(is_retryable(&BatchError::Status(503, String::new())));
        assert!(is_retryable(&BatchError::Status(429, String::new())));
        assert!(!is_retryable(&BatchError::Status(400, String::new())));
        assert!(!is_retryable(&BatchError::Auth));
        assert!(!is_retryable(&BatchError::BadBody("x".into())));
    }

    #[test]
    fn auth_statuses_map_to_auth() {
        assert_eq!(status_error(401, "{}"), BatchError::Auth);
        assert_eq!(status_error(403, "{}"), BatchError::Auth);
        assert_eq!(
            status_error(400, r#"{"err_code":"BAD","err_msg":"no audio"}"#),
            BatchError::Status(400, "BAD: no audio".into())
        );
    }

    #[test]
    fn timeout_grows_with_audio() {
        assert_eq!(timeout_for(0), Duration::from_secs(6));
        assert_eq!(timeout_for(10_000), Duration::from_secs(9));
    }
}
