//! Deepgram: request parameters and JSON parsing, shared by the stream and the batch client.

pub mod batch;
pub mod stream;

use serde::Deserialize;

const MODEL: &str = "nova-3";

/// Options both passes send, so pass 1 and pass 2 hear the same settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    pub language: String,
    pub keyterms: Vec<String>,
    pub dictation: bool,
    pub mip_opt_out: bool,
}

impl Params {
    /// The URL query string. Streaming adds the raw-audio format and live-result options.
    pub fn query(&self, streaming: bool) -> String {
        let mut q = vec![
            format!("model={MODEL}"),
            format!("language={}", urlencode(&self.language)),
            "smart_format=true".to_string(),
        ];
        if self.dictation {
            q.push("dictation=true".into());
        }
        if self.mip_opt_out {
            q.push("mip_opt_out=true".into());
        }
        if streaming {
            q.extend(
                [
                    "encoding=linear16",
                    "sample_rate=16000",
                    "channels=1",
                    "interim_results=true",
                    "endpointing=300",
                    "utterance_end_ms=1000",
                    "vad_events=true",
                ]
                .map(String::from),
            );
        }
        q.extend(self.keyterms.iter().map(|k| format!("keyterm={}", urlencode(k))));
        q.join("&")
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A text message from the live stream.
#[derive(Debug, PartialEq)]
pub enum Message {
    Transcript { text: String, is_final: bool },
    UtteranceEnd,
    SpeechStarted,
    Metadata,
    Error(String),
    Unknown,
}

#[derive(Deserialize)]
struct Raw {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    is_final: bool,
    /// An object in `Results`, an array in `UtteranceEnd`.
    channel: Option<serde_json::Value>,
    #[serde(flatten)]
    error: ErrorBody,
}

#[derive(Deserialize)]
struct Channel {
    #[serde(default)]
    alternatives: Vec<Alternative>,
}

#[derive(Deserialize)]
struct Alternative {
    #[serde(default)]
    transcript: String,
}

/// Deepgram error bodies: `{"err_code":"INVALID_AUTH","err_msg":"..."}` on HTTP,
/// `{"type":"Error","description":"..."}` on the stream.
#[derive(Deserialize, Default)]
struct ErrorBody {
    err_code: Option<String>,
    err_msg: Option<String>,
    description: Option<String>,
    message: Option<String>,
}

impl ErrorBody {
    fn text(&self) -> Option<String> {
        let detail = self.err_msg.as_ref().or(self.description.as_ref()).or(self.message.as_ref());
        match (&self.err_code, detail) {
            (Some(c), Some(d)) => Some(format!("{c}: {d}")),
            (Some(x), None) | (None, Some(x)) => Some(x.clone()),
            (None, None) => None,
        }
    }
}

pub fn parse_message(json: &str) -> Message {
    let Ok(raw) = serde_json::from_str::<Raw>(json) else { return Message::Unknown };
    match raw.kind.as_str() {
        "Results" => {
            let text = raw
                .channel
                .and_then(|c| serde_json::from_value::<Channel>(c).ok())
                .and_then(|c| c.alternatives.into_iter().next())
                .map(|a| a.transcript);
            Message::Transcript { text: text.unwrap_or_default(), is_final: raw.is_final }
        }
        "UtteranceEnd" => Message::UtteranceEnd,
        "SpeechStarted" => Message::SpeechStarted,
        "Metadata" => Message::Metadata,
        _ => match raw.error.text() {
            Some(e) => Message::Error(e),
            None => Message::Unknown,
        },
    }
}

/// The error text of a Deepgram HTTP error body, or the raw body when it is not JSON.
pub fn parse_error_body(body: &str) -> String {
    serde_json::from_str::<ErrorBody>(body).ok().and_then(|e| e.text()).unwrap_or_else(|| body.trim().chars().take(200).collect())
}

/// The transcript of a pre-recorded response.
pub fn parse_batch(json: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Resp {
        results: Results,
    }
    #[derive(Deserialize)]
    struct Results {
        #[serde(default)]
        channels: Vec<Channel>,
    }
    let resp: Resp = serde_json::from_str(json).map_err(|e| format!("bad response: {e}"))?;
    Ok(resp
        .results
        .channels
        .into_iter()
        .next()
        .and_then(|c| c.alternatives.into_iter().next())
        .map(|a| a.transcript)
        .unwrap_or_default())
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Failure {
    /// 401 / 403: the key is wrong. Retrying cannot help.
    Auth,
    /// 402: the key is out of credit.
    Credit,
    /// 429: too many requests on this key.
    RateLimited,
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params { language: "en".into(), keyterms: vec!["ELF".into(), "git rebase".into()], dictation: false, mip_opt_out: true }
    }

    #[test]
    fn stream_query_has_the_expected_parameters() {
        assert_eq!(
            params().query(true),
            "model=nova-3&language=en&smart_format=true&mip_opt_out=true&encoding=linear16&sample_rate=16000\
             &channels=1&interim_results=true&endpointing=300&utterance_end_ms=1000&vad_events=true\
             &keyterm=ELF&keyterm=git%20rebase"
        );
    }

    #[test]
    fn batch_query_has_no_stream_options() {
        let mut p = params();
        p.dictation = true;
        p.mip_opt_out = false;
        assert_eq!(p.query(false), "model=nova-3&language=en&smart_format=true&dictation=true&keyterm=ELF&keyterm=git%20rebase");
    }

    #[test]
    fn results_interim_and_final() {
        let interim = r#"{"type":"Results","channel_index":[0,1],"duration":1.0,"start":0.0,"is_final":false,
            "speech_final":false,"channel":{"alternatives":[{"transcript":"hello wor","confidence":0.9,"words":[]}]}}"#;
        assert_eq!(parse_message(interim), Message::Transcript { text: "hello wor".into(), is_final: false });
        let fin = r#"{"type":"Results","is_final":true,"speech_final":true,
            "channel":{"alternatives":[{"transcript":"Hello world."}]}}"#;
        assert_eq!(parse_message(fin), Message::Transcript { text: "Hello world.".into(), is_final: true });
    }

    #[test]
    fn other_stream_messages() {
        assert_eq!(parse_message(r#"{"type":"UtteranceEnd","channel":[0,1],"last_word_end":2.4}"#), Message::UtteranceEnd);
        assert_eq!(parse_message(r#"{"type":"SpeechStarted","timestamp":0.5}"#), Message::SpeechStarted);
        assert_eq!(parse_message(r#"{"type":"Metadata","request_id":"x"}"#), Message::Metadata);
        assert_eq!(parse_message("garbage"), Message::Unknown);
    }

    #[test]
    fn error_bodies() {
        assert_eq!(
            parse_message(r#"{"type":"Error","description":"Audio too short","message":"x"}"#),
            Message::Error("Audio too short".into())
        );
        let http = r#"{"err_code":"INVALID_AUTH","err_msg":"Invalid credentials.","request_id":"r"}"#;
        assert_eq!(parse_error_body(http), "INVALID_AUTH: Invalid credentials.");
        assert_eq!(parse_error_body("<html>bad gateway</html>"), "<html>bad gateway</html>");
    }

    #[test]
    fn batch_response() {
        let ok = r#"{"metadata":{},"results":{"channels":[{"alternatives":[{"transcript":"Hello.","confidence":0.99}]}]}}"#;
        assert_eq!(parse_batch(ok), Ok("Hello.".into()));
        assert_eq!(parse_batch(r#"{"results":{"channels":[]}}"#), Ok(String::new()));
        assert!(parse_batch("nope").is_err());
    }
}
