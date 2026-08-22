//! Networked inference: the HTTP server that lets one Handy install offer its
//! transcription engine to other machines.
//!
//! The motivating case is a two-machine setup: a desktop with a dedicated GPU
//! runs a large, accurate model and serves it; a laptop without one stops being
//! limited to small CPU models. The serving machine stays fully usable — server
//! mode is orthogonal to [`crate::settings::InferenceMode`].
//!
//! # Surface
//!
//! Two layers share one router:
//!
//! * **OpenAI-compatible** (`/v1/…`) — the interop contract. Any client that can
//!   talk to the OpenAI audio API, whisper.cpp's server, or faster-whisper works
//!   unchanged, and conversely Handy's client mode works against those servers.
//! * **Handy extensions** (`/handy/v1/…`) — what the OpenAI shape has no room
//!   for: capability discovery and the streaming session that carries live
//!   partial text.
//!
//! # Concurrency
//!
//! [`crate::managers::transcription::TranscriptionManager`] owns a single engine
//! behind a mutex, so inference is inherently serial. Requests are funnelled
//! through a one-permit semaphore with a bounded wait so a queue of clients
//! degrades into latency rather than into lock contention with the local user's
//! own dictation.

mod auth;
mod openai;
mod streaming;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use log::{error, info, warn};
use tauri::{AppHandle, Manager};
use tokio::sync::{oneshot, Semaphore};

use crate::managers::model::ModelManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::{get_settings, write_settings, AppSettings, SecretString};

pub use streaming::{StreamEvent, StreamEventBus};

/// Forward a live streaming event to any connected SSE client.
///
/// Called from the transcription manager's emit path, which also drives the
/// local overlay. Cheap when nothing is connected: a Tauri state lookup and a
/// broadcast send with no subscribers.
pub fn publish_stream_event(app: &AppHandle, event: StreamEvent) {
    if let Some(handle) = app.try_state::<Arc<ServerHandle>>() {
        handle.bus().publish(event);
    }
}

/// How long a queued request waits for the engine before giving up with 503.
/// Long enough to absorb one in-flight transcription, short enough that a client
/// gets a clear error instead of an apparent hang.
const ENGINE_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

/// Shared state handed to every route handler.
pub struct ServerState {
    pub app: AppHandle,
    pub transcription: Arc<TranscriptionManager>,
    pub models: Arc<ModelManager>,
    /// Serialises inference across concurrent HTTP clients; see module docs.
    /// Owned permits are required because a streaming session holds the engine
    /// across several requests, outliving any borrow of the state.
    pub engine_slot: Arc<Semaphore>,
    /// Bearer token requests must present. Captured at bind time so a settings
    /// edit cannot silently widen access on a running listener — changing the
    /// token restarts the server.
    pub token: String,
    pub streams: Arc<streaming::StreamRegistry>,
}

impl ServerState {
    /// Acquire the inference slot, or report why the caller should retry later.
    pub async fn acquire_engine(
        &self,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, auth::ApiError> {
        let slot = Arc::clone(&self.engine_slot);
        match tokio::time::timeout(ENGINE_WAIT, slot.acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(auth::ApiError::unavailable("server is shutting down")),
            Err(_) => Err(auth::ApiError::unavailable(
                "transcription engine busy; try again",
            )),
        }
    }
}

/// A running listener. Dropping this does nothing; call [`ServerHandle::stop`].
struct RunningServer {
    shutdown: oneshot::Sender<()>,
    addr: SocketAddr,
}

/// Tauri-managed handle owning at most one listener. Lives for the whole process
/// so the server can be toggled from settings without restarting Handy.
#[derive(Default)]
pub struct ServerHandle {
    running: std::sync::Mutex<Option<RunningServer>>,
    bus: Arc<StreamEventBus>,
}

impl ServerHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Broadcast channel the streaming worker publishes live text into. Always
    /// present, even when the server is stopped: publishing to it with no
    /// subscribers is a no-op, which keeps the hot path free of branches on
    /// server state.
    pub fn bus(&self) -> Arc<StreamEventBus> {
        Arc::clone(&self.bus)
    }

    pub fn is_running(&self) -> bool {
        self.running.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Address the listener actually bound, if running.
    pub fn bound_addr(&self) -> Option<SocketAddr> {
        self.running
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|r| r.addr))
    }

    fn stop(&self) {
        let taken = match self.running.lock() {
            Ok(mut guard) => guard.take(),
            // A poisoned lock means a previous stop panicked; there is nothing
            // left to signal and panicking again would take down the caller.
            Err(mut poisoned) => poisoned.get_mut().take(),
        };
        if let Some(server) = taken {
            info!("Stopping inference server on {}", server.addr);
            // The receiver is dropped only when the serve task has already
            // exited, so a send error here is benign.
            let _ = server.shutdown.send(());
        }
    }
}

/// Bring the listener in line with the current settings. Safe to call
/// repeatedly — it is the single entry point used at startup and after every
/// settings change.
pub fn apply_server_settings(app: &AppHandle) {
    let settings = get_settings(app);
    let handle = app.state::<Arc<ServerHandle>>();

    let desired = if settings.server_enabled {
        Some(bind_addr(&settings))
    } else {
        None
    };

    match (desired, handle.bound_addr()) {
        // Already listening where we want to be.
        (Some(want), Some(current)) if want == current => return,
        (None, None) => return,
        _ => {}
    }

    handle.stop();

    let Some(addr) = desired else { return };

    if settings.server_token.is_empty() {
        // Refuse to listen without a token rather than briefly exposing an
        // unauthenticated engine. `ensure_server_token` normally prevents this.
        warn!("Inference server enabled without a token; not starting");
        return;
    }

    if let Err(err) = start(app, addr, settings.server_token.expose().to_string()) {
        error!("Failed to start inference server on {addr}: {err}");
    }
}

fn bind_addr(settings: &AppSettings) -> SocketAddr {
    let ip = if settings.server_expose_lan {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    };
    SocketAddr::new(ip, settings.server_port)
}

/// Generate and persist a bearer token if none exists yet, returning the token
/// in use. Called when the user enables the server so the UI always has a token
/// to display.
pub fn ensure_server_token(app: &AppHandle) -> String {
    let settings = get_settings(app);
    if !settings.server_token.is_empty() {
        return settings.server_token.expose().to_string();
    }

    let token = generate_token();
    let mut updated = settings;
    updated.server_token = SecretString::new(token.clone());
    write_settings(app, updated);
    token
}

/// 256 bits of OS randomness, hex encoded. Long enough that the endpoint cannot
/// be brute-forced over a LAN, short enough to copy by hand.
fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn start(app: &AppHandle, addr: SocketAddr, token: String) -> anyhow::Result<()> {
    let transcription = Arc::clone(&*app.state::<Arc<TranscriptionManager>>());
    let models = Arc::clone(&*app.state::<Arc<ModelManager>>());
    let handle = Arc::clone(&*app.state::<Arc<ServerHandle>>());

    let state = Arc::new(ServerState {
        app: app.clone(),
        transcription,
        models,
        engine_slot: Arc::new(Semaphore::new(1)),
        token,
        streams: Arc::new(streaming::StreamRegistry::new(handle.bus())),
    });

    let router = build_router(Arc::clone(&state));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    // Bind synchronously so a port conflict surfaces to the caller (and the UI)
    // instead of disappearing into a spawned task.
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let bound = listener.local_addr()?;

    let serve_handle = Arc::clone(&handle);
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(err) => {
                error!("Failed to adopt listener socket: {err}");
                return;
            }
        };

        info!("Inference server listening on {bound}");
        let result = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;

        if let Err(err) = result {
            error!("Inference server stopped with an error: {err}");
        }

        // Clear the handle only if it still describes *this* listener; a restart
        // may already have installed a newer one.
        if let Ok(mut guard) = serve_handle.running.lock() {
            if guard.as_ref().map(|r| r.addr) == Some(bound) {
                *guard = None;
            }
        }
    });

    if let Ok(mut guard) = handle.running.lock() {
        *guard = Some(RunningServer {
            shutdown: shutdown_tx,
            addr: bound,
        });
    }

    Ok(())
}

fn build_router(state: Arc<ServerState>) -> Router {
    Router::new()
        // --- OpenAI-compatible surface -----------------------------------
        .route(
            "/v1/audio/transcriptions",
            post(openai::transcriptions).layer(axum::extract::DefaultBodyLimit::max(
                openai::MAX_UPLOAD_BYTES,
            )),
        )
        // Translation shares the handler; it only forces English output.
        .route(
            "/v1/audio/translations",
            post(openai::translations).layer(axum::extract::DefaultBodyLimit::max(
                openai::MAX_UPLOAD_BYTES,
            )),
        )
        .route("/v1/models", get(openai::models))
        // --- Handy extensions --------------------------------------------
        .route("/handy/v1/info", get(openai::info))
        .route("/handy/v1/stream", post(streaming::open_session))
        .route("/handy/v1/stream/{id}/audio", post(streaming::push_audio))
        .route("/handy/v1/stream/{id}/events", get(streaming::events))
        .route("/handy/v1/stream/{id}/finalize", post(streaming::finalize))
        .route("/handy/v1/stream/{id}/cancel", post(streaming::cancel))
        // Auth wraps every route above. `/health` is registered after the layer
        // so it stays reachable without a token — a client's "test connection"
        // needs to distinguish "wrong token" from "nothing listening".
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_token,
        ))
        .route("/health", get(openai::health))
        .with_state(state)
}
