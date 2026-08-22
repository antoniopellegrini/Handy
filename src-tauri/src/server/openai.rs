//! The OpenAI-compatible half of the inference server, plus the small Handy
//! discovery endpoints that share its request shape.
//!
//! Deliberate deviations from the OpenAI audio API, all in the direction of
//! being honest rather than silently wrong:
//!
//! * Only WAV uploads are decoded. Handy links no mp3/flac/ogg decoder, and
//!   guessing at a container it cannot read would surface as garbage text; an
//!   explicit 415 tells the caller to convert. Handy's own client always sends
//!   WAV.
//! * `timestamp_granularities`, `prompt` and `temperature` are accepted and
//!   ignored — the engine exposes no equivalent knob per request.
//! * `model` selects among *downloaded* models on the serving machine. An
//!   unknown id is a 404 rather than a download: pulling multi-gigabyte weights
//!   is not something a remote caller should be able to trigger.

use std::sync::Arc;

use axum::extract::{Multipart, State};
use axum::Json;
use log::{debug, info};
use serde::Serialize;
use serde_json::{json, Value};

use super::auth::ApiError;
use super::ServerState;
use crate::audio_toolkit::decode_wav_bytes;
use crate::managers::transcription::TranscribeOverrides;
use crate::settings::get_settings;

/// Upload ceiling. At 16 kHz mono 16-bit this is roughly 8 hours of audio —
/// far past any dictation, while still bounding what one request can allocate.
pub const MAX_UPLOAD_BYTES: usize = 1024 * 1024 * 1024;

/// What the client asked for, parsed out of the multipart form.
struct TranscriptionRequest {
    audio: Vec<f32>,
    model: Option<String>,
    language: Option<String>,
    /// `json` (default), `text`, or `verbose_json`.
    response_format: String,
}

#[derive(Serialize)]
struct TranscriptionResponse {
    text: String,
}

/// `POST /v1/audio/transcriptions`
pub async fn transcriptions(
    State(state): State<Arc<ServerState>>,
    multipart: Multipart,
) -> Result<axum::response::Response, ApiError> {
    run(state, multipart, None).await
}

/// `POST /v1/audio/translations` — identical, but the output language is forced
/// to English. Only meaningful on models that advertise translation; on others
/// the engine ignores it, matching local behaviour.
pub async fn translations(
    State(state): State<Arc<ServerState>>,
    multipart: Multipart,
) -> Result<axum::response::Response, ApiError> {
    run(state, multipart, Some(true)).await
}

async fn run(
    state: Arc<ServerState>,
    multipart: Multipart,
    force_translate: Option<bool>,
) -> Result<axum::response::Response, ApiError> {
    let request = parse_request(multipart).await?;
    let sample_count = request.audio.len();

    // Hold the engine slot across model load *and* inference, so a second
    // request cannot swap the model out from under this one.
    let _permit = state.acquire_engine().await?;

    if let Some(model) = request.model.as_deref().filter(|m| !m.is_empty()) {
        ensure_model_loaded(&state, model)?;
    }

    let overrides = TranscribeOverrides {
        language: request.language.clone(),
        translate_to_english: force_translate,
        // The server must never chain a request onward to another server: a
        // machine configured as a client would otherwise proxy in a loop.
        force_local: true,
    };

    info!(
        "Serving transcription: {} samples ({:.1}s), model={:?}, language={:?}",
        sample_count,
        sample_count as f32 / crate::audio_toolkit::TARGET_SAMPLE_RATE as f32,
        request.model,
        request.language,
    );

    let transcription = Arc::clone(&state.transcription);
    let audio = request.audio;
    // Inference is CPU/GPU-bound and blocking; keep it off the async runtime's
    // worker threads or the listener stops accepting connections.
    let text = tauri::async_runtime::spawn_blocking(move || {
        transcription.transcribe_with(audio, overrides)
    })
    .await
    .map_err(|err| ApiError::internal(format!("transcription task failed: {err}")))?
    .map_err(|err| ApiError::internal(format!("transcription failed: {err}")))?;

    Ok(render(text, &request.response_format))
}

/// Render the transcript in the format the caller asked for.
fn render(text: String, response_format: &str) -> axum::response::Response {
    use axum::response::IntoResponse;

    match response_format {
        "text" => text.into_response(),
        // `verbose_json` callers expect segments and timings. Handy's engines do
        // not surface those here, so the response carries the fields it can
        // honestly fill and omits the rest rather than inventing timings.
        "verbose_json" => Json(json!({
            "task": "transcribe",
            "text": text,
        }))
        .into_response(),
        _ => Json(TranscriptionResponse { text }).into_response(),
    }
}

/// Load `model_id` if it is not already the active one.
///
/// Callers must hold the engine permit: this mutates process-wide state that
/// the serving machine's own dictation also reads.
pub(super) fn ensure_model_loaded(state: &ServerState, model_id: &str) -> Result<(), ApiError> {
    if state.transcription.get_current_model().as_deref() == Some(model_id) {
        return Ok(());
    }

    let info = state
        .models
        .get_model_info(model_id)
        .ok_or_else(|| ApiError::not_found(format!("unknown model '{model_id}'")))?;

    if !info.is_downloaded {
        return Err(ApiError::not_found(format!(
            "model '{model_id}' is not downloaded on the server"
        )));
    }

    debug!("Switching served model to {model_id}");
    state
        .transcription
        .load_model(model_id)
        .map_err(|err| ApiError::internal(format!("failed to load model '{model_id}': {err}")))
}

/// Pull the audio and options out of the multipart body.
async fn parse_request(mut multipart: Multipart) -> Result<TranscriptionRequest, ApiError> {
    let mut audio: Option<Vec<f32>> = None;
    let mut model = None;
    let mut language = None;
    let mut response_format = "json".to_string();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| ApiError::bad_request(format!("malformed multipart body: {err}")))?
    {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" => {
                let filename = field.file_name().unwrap_or_default().to_lowercase();
                let bytes = field.bytes().await.map_err(|err| {
                    ApiError::bad_request(format!("could not read upload: {err}"))
                })?;

                // Sniff the RIFF/WAVE magic rather than trusting the extension:
                // a mislabelled upload should fail with "unsupported format",
                // not with a confusing parse error deeper down.
                if !is_wav(&bytes) {
                    return Err(ApiError::unsupported_media(format!(
                        "only WAV uploads are supported (got '{}'); \
                         convert with `ffmpeg -i in.mp3 -ar 16000 -ac 1 out.wav`",
                        if filename.is_empty() {
                            "unknown"
                        } else {
                            &filename
                        }
                    )));
                }

                audio = Some(
                    decode_wav_bytes(&bytes)
                        .map_err(|err| ApiError::bad_request(format!("invalid WAV: {err}")))?,
                );
            }
            "model" => model = text_field(field).await?,
            // OpenAI sends "" for auto-detect; normalise that to Handy's "auto".
            "language" => {
                language = text_field(field).await?.map(|value| {
                    if value.is_empty() {
                        "auto".into()
                    } else {
                        value
                    }
                })
            }
            "response_format" => {
                if let Some(value) = text_field(field).await? {
                    response_format = value;
                }
            }
            // Accepted-and-ignored: see module docs.
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let audio = audio.ok_or_else(|| ApiError::bad_request("missing 'file' field"))?;
    if audio.is_empty() {
        return Err(ApiError::bad_request("upload contained no audio samples"));
    }

    Ok(TranscriptionRequest {
        audio,
        model,
        language,
        response_format,
    })
}

async fn text_field(
    field: axum::extract::multipart::Field<'_>,
) -> Result<Option<String>, ApiError> {
    let name = field.name().unwrap_or("field").to_string();
    field
        .text()
        .await
        .map(|value| Some(value.trim().to_string()))
        .map_err(|err| ApiError::bad_request(format!("invalid '{name}' field: {err}")))
}

/// RIFF container with a WAVE form type.
fn is_wav(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

/// `GET /v1/models` — downloaded models only, in OpenAI's list shape.
pub async fn models(State(state): State<Arc<ServerState>>) -> Json<Value> {
    let data: Vec<Value> = state
        .models
        .get_available_models()
        .into_iter()
        .filter(|model| model.is_downloaded)
        .map(|model| {
            json!({
                "id": model.id,
                "object": "model",
                "owned_by": "handy",
                // Non-standard but harmless extras, so a Handy client can show
                // a useful picker without a second round trip.
                "name": model.name,
                "supports_translation": model.supports_translation,
            })
        })
        .collect();

    Json(json!({ "object": "list", "data": data }))
}

/// `GET /handy/v1/info` — capability discovery for Handy clients.
pub async fn info(State(state): State<Arc<ServerState>>) -> Json<Value> {
    let settings = get_settings(&state.app);
    let loaded = state.transcription.get_current_model();

    Json(json!({
        "server": "handy",
        "version": env!("CARGO_PKG_VERSION"),
        // Bumped when the streaming session protocol changes shape.
        "protocol": 1,
        "loaded_model": loaded,
        "default_model": settings.selected_model,
        "streaming": true,
        "accepts": ["audio/wav"],
    }))
}

/// `GET /health` — unauthenticated liveness probe. Deliberately reveals nothing
/// beyond "a Handy server is here", so an unauthenticated caller learns no
/// configuration details.
pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "server": "handy" }))
}
