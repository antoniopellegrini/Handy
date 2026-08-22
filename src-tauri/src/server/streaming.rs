//! Streaming transcription sessions: live partial text over the network.
//!
//! # Why two channels
//!
//! Server-Sent Events are one-directional (server → client), so they cannot
//! carry the microphone audio. A session therefore uses a channel per direction,
//! both plain HTTP:
//!
//! * **up** — `POST /handy/v1/stream/{id}/audio` with raw 16 kHz mono
//!   little-endian `i16` PCM. The client posts a chunk every few hundred
//!   milliseconds over a kept-alive connection.
//! * **down** — `GET /handy/v1/stream/{id}/events`, an SSE stream of partial
//!   text and phase changes.
//!
//! The alternative, a WebSocket, would be full-duplex in one connection but adds
//! a client-side dependency and its own reconnection semantics for no gain on a
//! LAN.
//!
//! # Lifecycle
//!
//! `open` → (`audio`* ‖ `events`) → `finalize` | `cancel`
//!
//! `open` takes the engine permit and holds it for the whole session, so a
//! session is mutually exclusive with batch requests and with other sessions.
//! An abandoned session would therefore wedge the server, so every session has a
//! watchdog that cancels it after [`SESSION_IDLE_TIMEOUT`] without traffic.
//!
//! # Event routing
//!
//! [`TranscriptionManager`](crate::managers::transcription::TranscriptionManager)
//! publishes live text into a process-wide [`StreamEventBus`] alongside the Tauri
//! events that drive the local overlay. The bus is not per-session because the
//! engine only ever runs one stream: while a remote session holds the permit the
//! manager's own guards refuse to start a second worker, so everything on the
//! bus during a session belongs to that session.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures_util::stream::Stream;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, OwnedSemaphorePermit};

use super::auth::ApiError;
use super::ServerState;
use crate::managers::transcription::{StreamPhase, StreamWorkKind, TranscribeOverrides};

/// A session with no audio and no finalize for this long is cancelled and its
/// engine permit released. Generous enough for a long pause mid-dictation,
/// short enough that a crashed client does not block the server for long.
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Bus capacity. Partial-text updates arrive a few times per second; this holds
/// several seconds of backlog for a slow SSE consumer before dropping the oldest
/// (which is harmless — each update is a full snapshot, not a delta).
const BUS_CAPACITY: usize = 64;

/// What a client sees on the SSE channel. Tagged so a client can match on
/// `type` without positional guessing.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Live snapshot. `committed` is the stable prefix; `tentative` may still be
    /// rewritten. Mirrors the local overlay's contract exactly.
    Text {
        committed: String,
        tentative: String,
    },
    /// The engine moved out of listening into finalizing/post-processing.
    Phase {
        phase: StreamPhase,
        #[serde(skip_serializing_if = "Option::is_none")]
        kind: Option<StreamWorkKind>,
    },
    /// Terminal: the finalized transcript.
    Final { text: String },
    /// Terminal: the session failed or was cancelled.
    Error { message: String },
}

/// Process-wide fan-out of streaming events to connected SSE clients.
///
/// Always constructed, even with the server off: publishing with no subscribers
/// is a cheap no-op, which keeps the per-partial hot path free of any branch on
/// server state.
pub struct StreamEventBus {
    sender: broadcast::Sender<StreamEvent>,
}

impl Default for StreamEventBus {
    fn default() -> Self {
        let (sender, _) = broadcast::channel(BUS_CAPACITY);
        Self { sender }
    }
}

impl StreamEventBus {
    pub fn publish(&self, event: StreamEvent) {
        // Err means "no subscribers", the normal case for a local-only install.
        let _ = self.sender.send(event);
    }

    fn subscribe(&self) -> broadcast::Receiver<StreamEvent> {
        self.sender.subscribe()
    }
}

/// One in-flight session.
struct Session {
    /// Held for the session's whole life so nothing else touches the engine.
    /// Never read — dropping it with the session is the point.
    _permit: OwnedSemaphorePermit,
    /// Subscribed at open time so no event emitted before the client's SSE
    /// request arrives is lost. Taken by the first `events` request.
    events: Mutex<Option<broadcast::Receiver<StreamEvent>>>,
    /// Millis since the epoch of the last request touching this session.
    last_seen: AtomicU64,
    /// Set by finalize/cancel so the watchdog does not act on a session that is
    /// already being torn down.
    closed: AtomicBool,
}

impl Session {
    fn touch(&self) {
        self.last_seen.store(now_ms(), Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.last_seen.load(Ordering::Relaxed)))
    }
}

/// The server's live sessions. At most one can exist at a time in practice (the
/// engine permit enforces it), but the map keeps the id → session lookup honest
/// rather than relying on that invariant.
pub struct StreamRegistry {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    bus: Arc<StreamEventBus>,
}

impl StreamRegistry {
    pub fn new(bus: Arc<StreamEventBus>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            bus,
        }
    }

    fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.lock().get(id).cloned()
    }

    fn remove(&self, id: &str) -> Option<Arc<Session>> {
        self.lock().remove(id)
    }

    /// Sessions are short-lived and the lock is only ever held for a map
    /// operation, so recovering from poisoning is preferable to propagating a
    /// panic into an HTTP handler.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct OpenSessionRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

/// `POST /handy/v1/stream`
pub async fn open_session(
    State(state): State<Arc<ServerState>>,
    body: Option<Json<OpenSessionRequest>>,
) -> Result<Json<Value>, ApiError> {
    let request = body.map(|Json(body)| body).unwrap_or_default();

    let permit = state.acquire_engine().await?;

    if let Some(model) = request.model.as_deref().filter(|m| !m.is_empty()) {
        super::openai::ensure_model_loaded(&state, model)?;
    }

    let id = new_session_id();
    let session = Arc::new(Session {
        _permit: permit,
        events: Mutex::new(Some(state.streams.bus.subscribe())),
        last_seen: AtomicU64::new(now_ms()),
        closed: AtomicBool::new(false),
    });

    state
        .streams
        .lock()
        .insert(id.clone(), Arc::clone(&session));

    // The manager decides whether a live stream is actually possible (it needs a
    // streaming-capable model); a no-op here simply means finalize will report
    // a fallback and the client re-sends the audio as a batch request.
    //
    // The session's language overrides the serving machine's own preference, and
    // `force_local` stops a server that is itself in client mode from proxying
    // the session to a third machine.
    state.transcription.start_stream_with(TranscribeOverrides {
        language: request.language.filter(|language| !language.is_empty()),
        translate_to_english: None,
        force_local: true,
    });

    spawn_watchdog(Arc::clone(&state), id.clone());

    info!("Opened streaming session {id}");
    Ok(Json(json!({ "session_id": id })))
}

/// `POST /handy/v1/stream/{id}/audio` — raw 16 kHz mono little-endian i16 PCM.
pub async fn push_audio(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let session = state
        .streams
        .get(&id)
        .ok_or_else(|| ApiError::not_found("unknown or expired session"))?;
    session.touch();

    if !body.len().is_multiple_of(2) {
        // A split sample means the client's framing is broken; silently dropping
        // the odd byte would desynchronise every subsequent chunk.
        return Err(ApiError::bad_request(
            "PCM chunk length must be a multiple of 2 (16-bit samples)",
        ));
    }

    let frame: Vec<f32> = body
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / i16::MAX as f32)
        .collect();

    let samples = frame.len();
    state.transcription.stream_router().feed(&frame);

    Ok(Json(json!({ "accepted_samples": samples })))
}

/// `GET /handy/v1/stream/{id}/events` — SSE channel of partials and phases.
pub async fn events(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, ApiError> {
    let session = state
        .streams
        .get(&id)
        .ok_or_else(|| ApiError::not_found("unknown or expired session"))?;
    session.touch();

    let receiver = session
        .events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .ok_or_else(|| ApiError::bad_request("event stream already consumed for this session"))?;

    // The unfold state is `Option<Receiver>`: a terminal event is yielded with a
    // `None` follow-on state, so the client receives it and *then* the response
    // closes. Carrying the receiver in the state (rather than breaking out of
    // the loop) is what makes that two-step possible.
    let stream = futures_util::stream::unfold(Some(receiver), |state| async move {
        let mut receiver = state?;
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let terminal =
                        matches!(event, StreamEvent::Final { .. } | StreamEvent::Error { .. });
                    // Serialisation of these types cannot fail; a comment-only
                    // event keeps the stream alive if it somehow did.
                    let sse = Event::default()
                        .json_data(&event)
                        .unwrap_or_else(|_| Event::default().comment("unserialisable event"));
                    return Some((Ok(sse), if terminal { None } else { Some(receiver) }));
                }
                // The consumer fell behind; each event is a full snapshot, so
                // skipping to the latest loses nothing but staleness.
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    debug!("SSE consumer lagged, dropped {skipped} events");
                }
                // Sender gone: the server is shutting down.
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// `POST /handy/v1/stream/{id}/finalize`
///
/// Returns the transcript, or `{"fallback": true}` when no live stream produced
/// text — the model was not streaming-capable, or the session carried no usable
/// audio. The client then re-sends the recording as a batch request, exactly as
/// the local path falls back from `finalize_stream` to `transcribe`.
pub async fn finalize(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let session = state
        .streams
        .remove(&id)
        .ok_or_else(|| ApiError::not_found("unknown or expired session"))?;
    session.closed.store(true, Ordering::Release);

    let transcription = Arc::clone(&state.transcription);
    let result = tauri::async_runtime::spawn_blocking(move || transcription.finalize_stream())
        .await
        .map_err(|err| ApiError::internal(format!("finalize task failed: {err}")))?;

    // The session (and its engine permit) is released when `session` drops at
    // the end of this function, whichever branch is taken.
    match result {
        Ok(Some(text)) if !text.trim().is_empty() => {
            state
                .streams
                .bus
                .publish(StreamEvent::Final { text: text.clone() });
            info!("Finalized streaming session {id}: {} chars", text.len());
            Ok(Json(json!({ "text": text })))
        }
        Ok(_) => {
            debug!("Streaming session {id} produced no text; telling client to fall back");
            state.streams.bus.publish(StreamEvent::Final {
                text: String::new(),
            });
            Ok(Json(json!({ "text": "", "fallback": true })))
        }
        Err(err) => {
            let message = format!("finalize failed: {err}");
            state.streams.bus.publish(StreamEvent::Error {
                message: message.clone(),
            });
            Err(ApiError::internal(message))
        }
    }
}

/// `POST /handy/v1/stream/{id}/cancel`
pub async fn cancel(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let session = state
        .streams
        .remove(&id)
        .ok_or_else(|| ApiError::not_found("unknown or expired session"))?;
    session.closed.store(true, Ordering::Release);

    state.transcription.cancel_stream();
    state.streams.bus.publish(StreamEvent::Error {
        message: "session cancelled".into(),
    });

    info!("Cancelled streaming session {id}");
    Ok(Json(json!({ "cancelled": true })))
}

/// Cancel a session that stops sending traffic, so a crashed or disconnected
/// client cannot hold the engine permit indefinitely.
fn spawn_watchdog(state: Arc<ServerState>, id: String) {
    tauri::async_runtime::spawn(async move {
        // Poll at a fraction of the timeout: the check is two atomic loads, and
        // this bounds how long past the deadline a dead session lingers.
        let tick = SESSION_IDLE_TIMEOUT / 8;
        loop {
            tokio::time::sleep(tick).await;

            let Some(session) = state.streams.get(&id) else {
                return; // Finalized or cancelled normally.
            };
            if session.closed.load(Ordering::Acquire) {
                return;
            }
            if session.idle_for() < SESSION_IDLE_TIMEOUT {
                continue;
            }

            warn!("Streaming session {id} idle for too long; cancelling");
            if let Some(session) = state.streams.remove(&id) {
                session.closed.store(true, Ordering::Release);
                state.transcription.cancel_stream();
                state.streams.bus.publish(StreamEvent::Error {
                    message: "session timed out".into(),
                });
            }
            return;
        }
    });
}

fn new_session_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
