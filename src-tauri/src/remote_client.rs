//! Client half of networked inference: offload transcription to a remote Handy
//! server (or any OpenAI-compatible speech-to-text endpoint) instead of running
//! a model locally.
//!
//! The point is the machine without a dedicated GPU. Locally it is stuck with
//! small, less accurate CPU models; pointed at a GPU box running Handy in server
//! mode it gets that machine's accuracy at LAN latency.
//!
//! # Two paths
//!
//! * **Batch** ([`transcribe`]) — the whole recording is uploaded as one WAV to
//!   `POST /v1/audio/transcriptions`. This is the OpenAI-compatible path, so it
//!   also works against whisper.cpp's server, faster-whisper, Groq or OpenAI
//!   itself.
//! * **Streaming** ([`StreamSession`]) — audio is posted in chunks while live
//!   partial text arrives over Server-Sent Events. Handy-specific, and the
//!   client falls back to batch whenever the server does not support it.
//!
//! Every entry point here is blocking and drives the async HTTP client through
//! [`tauri::async_runtime::block_on`]. That matches the callers: the recording
//! path already runs transcription on a dedicated thread or via
//! `spawn_blocking`, never on a runtime worker.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::audio_toolkit::encode_wav_bytes;
use crate::settings::AppSettings;

/// How much audio to accumulate before posting a streaming chunk. A quarter
/// second keeps the request rate low (4/s) while staying well under the delay at
/// which live partial text stops feeling live.
const CHUNK_SAMPLES: usize = crate::audio_toolkit::TARGET_SAMPLE_RATE as usize / 4;

/// Cap on control requests (open/finalize/cancel, connection tests). Inference
/// itself uses the user's configured timeout; these are bookkeeping round trips
/// that should fail fast when the server has gone away.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);

/// A remote server's self-description, as returned by `GET /handy/v1/info`.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct RemoteServerInfo {
    /// `"handy"` for a Handy server; absent or different for a generic
    /// OpenAI-compatible endpoint.
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub loaded_model: Option<String>,
    /// Whether the streaming session endpoints are available. Always false for a
    /// non-Handy server, which forces the batch path.
    #[serde(default)]
    pub streaming: bool,
}

/// One entry of `GET /v1/models`.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct RemoteModel {
    pub id: String,
    /// Handy servers add a display name; generic servers do not, and the id is
    /// used instead.
    #[serde(default)]
    pub name: Option<String>,
}

/// Where to reach the server and how long to wait, resolved from settings.
#[derive(Debug, Clone)]
pub struct RemoteTarget {
    pub base_url: String,
    pub token: String,
    pub model: String,
    pub timeout: Duration,
}

impl RemoteTarget {
    /// Build a target from settings, or explain what is missing.
    pub fn from_settings(settings: &AppSettings) -> Result<Self> {
        let base_url = normalize_base_url(&settings.client_base_url)?;
        Ok(Self {
            base_url,
            token: settings.client_token.expose().to_string(),
            model: settings.client_model.clone(),
            timeout: Duration::from_secs(settings.client_timeout_secs.max(1)),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

/// Trim and validate a user-typed server address.
///
/// Accepts a bare `host` or `host:port` and assumes plain HTTP, because that is
/// what a user copying an IP out of the server's settings panel will type.
/// Returns the URL without a trailing slash so paths can be concatenated.
fn normalize_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(anyhow!(
            "No server address configured. Set it in Settings → Networked inference."
        ));
    }

    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };

    // Parse to reject nonsense early, with a message naming the offending value
    // rather than surfacing a reqwest error at request time.
    let parsed = reqwest::Url::parse(&with_scheme)
        .with_context(|| format!("'{raw}' is not a valid server address"))?;
    if parsed.host_str().is_none() {
        return Err(anyhow!("'{raw}' has no host"));
    }

    Ok(with_scheme.trim_end_matches('/').to_string())
}

fn client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .context("failed to build HTTP client")
}

/// Client without a total-response timeout, for the SSE channel: a long silence
/// between partials is normal and must not kill the connection.
fn streaming_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONTROL_TIMEOUT)
        .build()
        .context("failed to build HTTP client")
}

/// Turn a non-2xx response into an error carrying the server's own message.
async fn error_for_status(response: reqwest::Response, context: &str) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    // Handy and OpenAI both wrap failures in {"error": {"message": ...}}; prefer
    // that message over the raw JSON so the overlay shows something readable.
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect());

    if message.is_empty() {
        anyhow!("{context}: server returned {status}")
    } else {
        anyhow!("{context}: {status} — {message}")
    }
}

// ---------------------------------------------------------------------------
// Batch
// ---------------------------------------------------------------------------

/// Transcribe a whole recording on the remote server.
///
/// Blocking: intended for the same call sites as the local engine.
pub fn transcribe(settings: &AppSettings, samples: Vec<f32>) -> Result<String> {
    let target = RemoteTarget::from_settings(settings)?;
    let language = settings.selected_language.clone();
    let translate = settings.translate_to_english;

    tauri::async_runtime::block_on(transcribe_async(target, samples, language, translate))
}

async fn transcribe_async(
    target: RemoteTarget,
    samples: Vec<f32>,
    language: String,
    translate: bool,
) -> Result<String> {
    let sample_count = samples.len();
    let wav = encode_wav_bytes(&samples).context("failed to encode audio for upload")?;

    // `/translations` is the OpenAI-compatible way to ask for English output;
    // there is no per-request flag on `/transcriptions`.
    let path = if translate {
        "/v1/audio/translations"
    } else {
        "/v1/audio/transcriptions"
    };

    let mut form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(wav)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .context("failed to build upload part")?,
    );

    // A generic OpenAI endpoint requires `model`; a Handy server treats an empty
    // value as "whatever you have loaded", so only send it when set.
    if !target.model.is_empty() {
        form = form.text("model", target.model.clone());
    }
    // OpenAI's convention for auto-detect is an omitted/empty language.
    if language != "auto" {
        form = form.text("language", language);
    }

    debug!(
        "Remote transcription: {} samples to {}",
        sample_count,
        target.url(path)
    );

    let response = client(target.timeout)?
        .post(target.url(path))
        .bearer_auth(&target.token)
        .multipart(form)
        .send()
        .await
        .with_context(|| {
            format!(
                "could not reach transcription server at {}",
                target.base_url
            )
        })?;

    if !response.status().is_success() {
        return Err(error_for_status(response, "remote transcription failed").await);
    }

    #[derive(Deserialize)]
    struct TextResponse {
        text: String,
    }

    let body = response
        .text()
        .await
        .context("failed to read server response")?;

    // `response_format` was not sent, so a compliant server replies with JSON;
    // fall back to treating the body as the transcript for servers that reply
    // with bare text anyway.
    Ok(match serde_json::from_str::<TextResponse>(&body) {
        Ok(parsed) => parsed.text,
        Err(_) => body.trim().to_string(),
    })
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Probe a server: reachable, token accepted, and what it can do.
///
/// Used by the settings UI's "Test connection". Returns a description even for a
/// non-Handy OpenAI-compatible endpoint, with `streaming: false`.
pub async fn probe(base_url: &str, token: &str) -> Result<RemoteServerInfo> {
    let base_url = normalize_base_url(base_url)?;
    let client = client(CONTROL_TIMEOUT)?;

    let response = client
        .get(format!("{base_url}/handy/v1/info"))
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("could not reach {base_url}"))?;

    if response.status().is_success() {
        return response
            .json::<RemoteServerInfo>()
            .await
            .context("server sent a malformed info response");
    }

    // 401 is a configuration problem the user must fix, not a reason to probe on
    // as if the server were merely a different flavour.
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(anyhow!("server rejected the token"));
    }

    // No Handy info endpoint: fall back to the OpenAI model list, which every
    // compatible server implements. Success there means a usable batch-only
    // server.
    let models = client
        .get(format!("{base_url}/v1/models"))
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("could not reach {base_url}"))?;

    if !models.status().is_success() {
        return Err(error_for_status(models, "server is not usable").await);
    }

    Ok(RemoteServerInfo {
        server: "openai-compatible".to_string(),
        version: String::new(),
        loaded_model: None,
        streaming: false,
    })
}

/// List the models the server can transcribe with.
pub async fn list_models(base_url: &str, token: &str) -> Result<Vec<RemoteModel>> {
    let base_url = normalize_base_url(base_url)?;

    let response = client(CONTROL_TIMEOUT)?
        .get(format!("{base_url}/v1/models"))
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("could not reach {base_url}"))?;

    if !response.status().is_success() {
        return Err(error_for_status(response, "could not list server models").await);
    }

    #[derive(Deserialize)]
    struct ModelList {
        #[serde(default)]
        data: Vec<RemoteModel>,
    }

    Ok(response
        .json::<ModelList>()
        .await
        .context("server sent a malformed model list")?
        .data)
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// A live streaming event received from the server, mirroring the server-side
/// `StreamEvent` shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteStreamEvent {
    Text {
        #[serde(default)]
        committed: String,
        #[serde(default)]
        tentative: String,
    },
    Phase {},
    Final {
        #[serde(default)]
        text: String,
    },
    Error {
        #[serde(default)]
        message: String,
    },
}

/// An open streaming session on a remote server.
///
/// Blocking by design: it is driven from the transcription manager's stream
/// worker thread, which owns the audio command channel and must not become
/// async. Audio is buffered locally and flushed a chunk at a time; live text
/// arrives on a background task that invokes the callback given to [`Self::open`].
pub struct StreamSession {
    target: RemoteTarget,
    id: String,
    client: reqwest::Client,
    /// Samples not yet posted. Flushed once [`CHUNK_SAMPLES`] accumulate so the
    /// request rate stays bounded regardless of the audio callback's frame size.
    pending: Vec<f32>,
    /// Set once a request fails, so the remaining pushes stop hammering a server
    /// that has gone away and finalize can report the original cause.
    failed: Option<String>,
}

impl StreamSession {
    /// Open a session and start consuming its event stream.
    ///
    /// `on_event` is invoked from a background task for every event the server
    /// sends; it is where the caller feeds the local overlay.
    pub fn open(
        settings: &AppSettings,
        on_event: impl Fn(RemoteStreamEvent) + Send + 'static,
    ) -> Result<Self> {
        let target = RemoteTarget::from_settings(settings)?;
        let language = settings.selected_language.clone();

        let client = client(target.timeout)?;
        let id = tauri::async_runtime::block_on(open_session(&client, &target, &language))?;

        info!(
            "Opened remote streaming session {id} on {}",
            target.base_url
        );

        spawn_event_reader(target.clone(), id.clone(), on_event);

        Ok(Self {
            target,
            id,
            client,
            pending: Vec::with_capacity(CHUNK_SAMPLES * 2),
            failed: None,
        })
    }

    /// Buffer a frame, posting a chunk once enough has accumulated.
    pub fn push(&mut self, frame: &[f32]) {
        if self.failed.is_some() {
            return;
        }
        self.pending.extend_from_slice(frame);
        if self.pending.len() >= CHUNK_SAMPLES {
            self.flush();
        }
    }

    /// Post whatever is buffered, recording the first failure.
    fn flush(&mut self) {
        if self.pending.is_empty() || self.failed.is_some() {
            return;
        }
        let chunk = std::mem::take(&mut self.pending);
        let body = pcm_bytes(&chunk);

        let result = tauri::async_runtime::block_on(
            self.client
                .post(
                    self.target
                        .url(&format!("/handy/v1/stream/{}/audio", self.id)),
                )
                .bearer_auth(&self.target.token)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(body)
                .send(),
        );

        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                let status = response.status();
                warn!("Remote stream chunk rejected with {status}");
                self.failed = Some(format!("server rejected audio chunk ({status})"));
            }
            Err(err) => {
                warn!("Remote stream chunk failed: {err}");
                self.failed = Some(format!("lost connection to server: {err}"));
            }
        }
    }

    /// Flush the tail, finalize the session, and return the transcript.
    ///
    /// `Ok(None)` means the server produced no usable text and the caller should
    /// fall back to a batch request — the same contract as the local
    /// `finalize_stream`.
    pub fn finalize(mut self) -> Result<Option<String>> {
        self.flush();
        if let Some(reason) = self.failed.take() {
            return Err(anyhow!(reason));
        }

        #[derive(Deserialize)]
        struct FinalizeResponse {
            #[serde(default)]
            text: String,
            #[serde(default)]
            fallback: bool,
        }

        let response = tauri::async_runtime::block_on(async {
            let response = self
                .client
                .post(
                    self.target
                        .url(&format!("/handy/v1/stream/{}/finalize", self.id)),
                )
                .bearer_auth(&self.target.token)
                .timeout(self.target.timeout)
                .send()
                .await
                .context("could not finalize remote stream")?;

            if !response.status().is_success() {
                return Err(error_for_status(response, "remote finalize failed").await);
            }

            response
                .json::<FinalizeResponse>()
                .await
                .context("server sent a malformed finalize response")
        })?;

        if response.fallback || response.text.trim().is_empty() {
            debug!(
                "Remote session {} produced no text; batch fallback",
                self.id
            );
            return Ok(None);
        }
        Ok(Some(response.text))
    }

    /// Abandon the session so the server releases its engine immediately rather
    /// than waiting for its idle watchdog.
    pub fn cancel(self) {
        let result = tauri::async_runtime::block_on(
            self.client
                .post(
                    self.target
                        .url(&format!("/handy/v1/stream/{}/cancel", self.id)),
                )
                .bearer_auth(&self.target.token)
                .timeout(CONTROL_TIMEOUT)
                .send(),
        );
        if let Err(err) = result {
            // Best effort: the server's watchdog reclaims the session anyway.
            debug!("Cancelling remote session {} failed: {err}", self.id);
        }
    }
}

async fn open_session(
    client: &reqwest::Client,
    target: &RemoteTarget,
    language: &str,
) -> Result<String> {
    #[derive(Deserialize)]
    struct OpenResponse {
        session_id: String,
    }

    let response = client
        .post(target.url("/handy/v1/stream"))
        .bearer_auth(&target.token)
        .timeout(CONTROL_TIMEOUT)
        .json(&serde_json::json!({
            "model": target.model,
            "language": language,
        }))
        .send()
        .await
        .with_context(|| format!("could not open a stream on {}", target.base_url))?;

    if !response.status().is_success() {
        return Err(error_for_status(response, "server refused the stream").await);
    }

    Ok(response
        .json::<OpenResponse>()
        .await
        .context("server sent a malformed session response")?
        .session_id)
}

/// Consume the session's SSE channel, handing each event to `on_event`.
fn spawn_event_reader(
    target: RemoteTarget,
    id: String,
    on_event: impl Fn(RemoteStreamEvent) + Send + 'static,
) {
    tauri::async_runtime::spawn(async move {
        let client = match streaming_client() {
            Ok(client) => client,
            Err(err) => {
                warn!("Could not build SSE client: {err}");
                return;
            }
        };

        let response = client
            .get(target.url(&format!("/handy/v1/stream/{id}/events")))
            .bearer_auth(&target.token)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await;

        let response = match response {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                // Not fatal to the session: audio still uploads and finalize
                // still returns the transcript, only the live preview is lost.
                warn!("Remote event stream unavailable ({})", response.status());
                return;
            }
            Err(err) => {
                warn!("Remote event stream failed to open: {err}");
                return;
            }
        };

        let mut stream = response.bytes_stream();
        // SSE frames are separated by a blank line and may split across chunks,
        // so bytes are accumulated and drained frame by frame.
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(err) => {
                    debug!("Remote event stream ended: {err}");
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(split) = buffer.find("\n\n") {
                let frame: String = buffer.drain(..split + 2).collect();
                let Some(event) = parse_sse_frame(&frame) else {
                    continue;
                };
                let terminal = matches!(
                    event,
                    RemoteStreamEvent::Final { .. } | RemoteStreamEvent::Error { .. }
                );
                on_event(event);
                if terminal {
                    return;
                }
            }
        }
    });
}

/// Extract the JSON payload from one SSE frame.
///
/// Only `data:` lines matter here; comments (keep-alives) and other fields are
/// ignored. A multi-line `data:` payload is joined with newlines per the spec.
fn parse_sse_frame(frame: &str) -> Option<RemoteStreamEvent> {
    let data: Vec<&str> = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .collect();

    if data.is_empty() {
        return None;
    }

    let payload = data.join("\n");
    match serde_json::from_str(&payload) {
        Ok(event) => Some(event),
        Err(err) => {
            debug!("Ignoring unparseable SSE payload: {err}");
            None
        }
    }
}

/// Encode f32 samples as little-endian 16-bit PCM, the streaming wire format.
/// Half the bytes of f32 with no audible loss for speech recognition.
fn pcm_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        let scaled = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        bytes.extend_from_slice(&scaled.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Accept exactly one request, hand back `body`, and report what the client
    /// actually sent. Returns the base URL plus a receiver for the raw request,
    /// so the tests can assert on the wire format rather than only on parsing.
    async fn serve_one(
        status: &str,
        body: &str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
            body.len()
        );
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // One read is enough: the assertions only look at the request line
            // and headers, which arrive in the first segment.
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).await.unwrap();
            let _ = tx.send(String::from_utf8_lossy(&buffer[..read]).to_string());
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        (format!("http://{address}"), rx)
    }

    fn target(base_url: String) -> RemoteTarget {
        RemoteTarget {
            base_url,
            token: "test-token".into(),
            model: "whisper-large".into(),
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn batch_transcription_uploads_wav_and_returns_the_text() {
        let (base_url, request) = serve_one("200 OK", r#"{"text":"ciao mondo"}"#).await;

        let text = transcribe_async(target(base_url), vec![0.1; 1600], "it".into(), false)
            .await
            .unwrap();

        assert_eq!(text, "ciao mondo");
        let sent = request.await.unwrap();
        assert!(sent.starts_with("POST /v1/audio/transcriptions"), "{sent}");
        assert!(sent.contains("authorization: Bearer test-token"), "{sent}");
        assert!(sent.contains("multipart/form-data"), "{sent}");
    }

    #[tokio::test]
    async fn translation_intent_uses_the_translations_endpoint() {
        let (base_url, request) = serve_one("200 OK", r#"{"text":"hello"}"#).await;

        transcribe_async(target(base_url), vec![0.1; 1600], "it".into(), true)
            .await
            .unwrap();

        let sent = request.await.unwrap();
        assert!(sent.starts_with("POST /v1/audio/translations"), "{sent}");
    }

    #[tokio::test]
    async fn auto_language_is_omitted_so_the_server_detects_it() {
        let (base_url, request) = serve_one("200 OK", r#"{"text":"x"}"#).await;

        transcribe_async(target(base_url), vec![0.1; 1600], "auto".into(), false)
            .await
            .unwrap();

        // OpenAI's convention for auto-detect is an absent `language` part;
        // sending the literal "auto" would be rejected as an unknown language.
        let sent = request.await.unwrap();
        assert!(!sent.contains("name=\"language\""), "{sent}");
    }

    #[tokio::test]
    async fn server_error_message_is_surfaced_to_the_caller() {
        let (base_url, _request) = serve_one(
            "404 Not Found",
            r#"{"error":{"message":"unknown model 'whisper-large'","type":"not_found_error"}}"#,
        )
        .await;

        let error = transcribe_async(target(base_url), vec![0.1; 1600], "auto".into(), false)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("unknown model 'whisper-large'"), "{error}");
    }

    #[tokio::test]
    async fn a_bare_text_response_is_accepted_as_the_transcript() {
        // whisper.cpp's server replies with plain text for some configurations;
        // treating the body as the transcript beats failing to parse it.
        let (base_url, _request) = serve_one("200 OK", "just some text").await;

        let text = transcribe_async(target(base_url), vec![0.1; 1600], "auto".into(), false)
            .await
            .unwrap();

        assert_eq!(text, "just some text");
    }

    #[test]
    fn base_url_accepts_bare_host_and_port() {
        assert_eq!(
            normalize_base_url("192.168.1.20:8756").unwrap(),
            "http://192.168.1.20:8756"
        );
    }

    #[test]
    fn base_url_preserves_explicit_scheme_and_strips_trailing_slash() {
        assert_eq!(
            normalize_base_url("https://gpu.lan:8756/").unwrap(),
            "https://gpu.lan:8756"
        );
    }

    #[test]
    fn base_url_rejects_empty_input() {
        assert!(normalize_base_url("   ").is_err());
    }

    #[test]
    fn pcm_round_trips_through_the_wire_format() {
        let samples = [0.0f32, 0.5, -0.5, 1.0, -1.0];
        let bytes = pcm_bytes(&samples);
        assert_eq!(bytes.len(), samples.len() * 2);

        let decoded: Vec<f32> = bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / i16::MAX as f32)
            .collect();
        for (original, decoded) in samples.iter().zip(decoded.iter()) {
            assert!((original - decoded).abs() < 1e-3, "{original} vs {decoded}");
        }
    }

    #[test]
    fn sse_frame_parses_a_text_event() {
        let frame =
            "data: {\"type\":\"text\",\"committed\":\"hello\",\"tentative\":\" world\"}\n\n";
        match parse_sse_frame(frame) {
            Some(RemoteStreamEvent::Text {
                committed,
                tentative,
            }) => {
                assert_eq!(committed, "hello");
                assert_eq!(tentative, " world");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn sse_keepalive_comment_yields_no_event() {
        assert!(parse_sse_frame(":keep-alive\n\n").is_none());
    }
}
