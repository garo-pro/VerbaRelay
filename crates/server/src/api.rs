//! The transcription API for callers that submit audio.
//!
//! `POST /api/v1/transcriptions` (multipart/form-data, `Authorization: Bearer <api token>`)
//!
//! | field     | meaning                                                         |
//! |-----------|-----------------------------------------------------------------|
//! | file      | the audio file (required)                                       |
//! | model     | model override (only if the server allows it)                   |
//! | language  | language code such as "de", or "auto" (default)                 |
//! | translate | "true" to translate into English                                |
//! | format    | json (default), text, srt or vtt                                |
//! | stream    | "true" for Server-Sent Events with live segments                |
//! | split     | "false" to never split the recording across peers               |

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::{Multipart, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};
use uuid::Uuid;
use verbarelay_protocol::models::ModelSpec;
use verbarelay_protocol::transcript::TranscriptResult;

use crate::app::{ApiError, SharedState};
use crate::audio;
use crate::auth;
use crate::db::JobRecord;
use crate::formats::{FinalTranscript, OutputFormat};
use crate::scheduler::{Job, JobEvent, JobHandle};

/// `GET /api/v1/health`: liveness probe without details.
pub async fn health(State(app): State<SharedState>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "peers": app.scheduler.peer_count(),
    }))
}

/// `GET /api/v1/status`: connected peers and their scores (API token required).
pub async fn status(State(app): State<SharedState>, headers: HeaderMap) -> Result<Json<serde_json::Value>, ApiError> {
    authenticate(&app, &headers)?;
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "policy": {
            "enforce": app.config.models.enforce,
            "default_model": app.config.models.default,
            "allow_request_override": app.config.models.allow_request_override,
            "allowed_models": app.config.allowed_models().into_iter().map(|m| m.name).collect::<Vec<_>>(),
        },
        "peers": app.scheduler.peer_statuses(),
    })))
}

fn authenticate(app: &SharedState, headers: &HeaderMap) -> Result<Uuid, ApiError> {
    let token = auth::bearer(headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()))
        .ok_or_else(ApiError::unauthorized)?;
    app.db
        .use_api_token(&auth::hash_token(token))
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::unauthorized)
}

/// Releases a token's active job slot when dropped.
struct ActiveJobGuard {
    app: SharedState,
    token_id: Uuid,
}

impl ActiveJobGuard {
    fn acquire(app: &SharedState, token_id: Uuid) -> Result<Self, ApiError> {
        let mut active = app.active_jobs.lock().unwrap();
        let count = active.entry(token_id).or_insert(0);
        if *count >= app.config.rate_limit.max_active_jobs_per_token {
            return Err(ApiError::too_many(
                "too many jobs of this token are queued or running",
                std::time::Duration::from_secs(5),
            ));
        }
        *count += 1;
        Ok(Self {
            app: app.clone(),
            token_id,
        })
    }
}

impl Drop for ActiveJobGuard {
    fn drop(&mut self) {
        let mut active = self.app.active_jobs.lock().unwrap();
        if let Some(count) = active.get_mut(&self.token_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                active.remove(&self.token_id);
            }
        }
    }
}

#[derive(Default)]
struct RequestFields {
    file: Option<(Vec<u8>, Option<String>)>,
    model: Option<String>,
    language: Option<String>,
    translate: bool,
    format: Option<String>,
    stream: bool,
    split: Option<bool>,
}

fn parse_bool(value: &str) -> Result<bool, ApiError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => Err(ApiError::bad_request(format!("expected a boolean, got '{other}'"))),
    }
}

async fn read_fields(mut multipart: Multipart) -> Result<RequestFields, ApiError> {
    let mut fields = RequestFields::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?
    {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            let file_name = field.file_name().map(str::to_string);
            let bytes = field.bytes().await.map_err(|e| ApiError::bad_request(e.to_string()))?;
            fields.file = Some((bytes.to_vec(), file_name));
            continue;
        }
        let value = field.text().await.map_err(|e| ApiError::bad_request(e.to_string()))?;
        match name.as_str() {
            "model" if !value.trim().is_empty() => fields.model = Some(value.trim().to_string()),
            "language" if !value.trim().is_empty() => fields.language = Some(value.trim().to_ascii_lowercase()),
            "translate" => fields.translate = parse_bool(&value)?,
            "format" => fields.format = Some(value),
            "stream" => fields.stream = parse_bool(&value)?,
            "split" => fields.split = Some(parse_bool(&value)?),
            _ => {}
        }
    }
    Ok(fields)
}

/// Applies the model policy. Returns the model and notices for the caller.
fn resolve_model(
    app: &SharedState,
    requested: Option<&str>,
) -> Result<(Option<ModelSpec>, Vec<String>, bool), ApiError> {
    let config = &app.config;
    let default = config
        .models
        .enforce
        .then(|| config.model(&config.models.default))
        .flatten();
    let Some(name) = requested else {
        return Ok((default, Vec::new(), false));
    };
    if !config.models.allow_request_override {
        let notice = format!(
            "The requested model '{name}' was not used: this server does not allow requests to override the model."
        );
        return Ok((default, vec![notice], true));
    }
    let allowed = config.allowed_models();
    match allowed.iter().find(|m| m.name == name) {
        Some(model) => Ok((Some(model.clone()), Vec::new(), false)),
        None => Err(ApiError::bad_request(format!(
            "model '{name}' is not allowed on this server; allowed models: {}",
            allowed.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

fn validate_language(language: Option<String>) -> Result<Option<String>, ApiError> {
    match language {
        None => Ok(None),
        Some(l) if l == "auto" => Ok(None),
        Some(l) if (2..=8).contains(&l.len()) && l.chars().all(|c| c.is_ascii_alphabetic() || c == '-') => Ok(Some(l)),
        Some(l) => Err(ApiError::bad_request(format!("invalid language code '{l}'"))),
    }
}

/// `POST /api/v1/transcriptions`
pub async fn transcribe(
    State(app): State<SharedState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let created_at = crate::db::now();
    let token_id = authenticate(&app, &headers)?;
    if let Err(wait) = app.api_limiter.check(&token_id) {
        return Err(ApiError::too_many("rate limit exceeded", wait));
    }
    let guard = ActiveJobGuard::acquire(&app, token_id)?;

    let fields = read_fields(multipart).await?;
    let (data, file_name) = fields
        .file
        .ok_or_else(|| ApiError::bad_request("the 'file' field is required"))?;
    let format = match fields.format.as_deref() {
        None => OutputFormat::Json,
        Some(f) => {
            OutputFormat::parse(f).ok_or_else(|| ApiError::bad_request("format must be json, text, srt or vtt"))?
        }
    };
    let language = validate_language(fields.language)?;
    let (model, notices, override_rejected) = resolve_model(&app, fields.model.as_deref())?;

    let pcm = tokio::task::spawn_blocking(move || audio::decode(data, file_name.as_deref()))
        .await
        .map_err(ApiError::internal)?
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let duration_ms = pcm.duration_ms();

    // Split long recordings across idle peers.
    let split = &app.config.splitting;
    let mut points = Vec::new();
    if split.enabled && fields.split != Some(false) && duration_ms >= split.min_duration_secs * 1000 {
        let idle = app.scheduler.idle_peers_for(model.as_ref());
        let by_length = (duration_ms / 1000 / split.min_part_secs) as usize;
        let parts = idle.min(split.max_parts).min(by_length);
        if parts >= 2 {
            points = audio::split_points(&pcm.samples, parts);
        }
    }

    let handle = Job::create(model.clone(), language, fields.translate, pcm.samples, &points);
    let job = handle.job.clone();
    info!(
        job = %job.id,
        token = %token_id,
        duration_s = duration_ms / 1000,
        parts = job.parts.len(),
        model = model.as_ref().map_or("peer default", |m| m.name.as_str()),
        "job queued"
    );
    app.scheduler.submit(job.clone());

    let context = JobContext {
        app,
        token_id,
        created_at,
        started,
        notices,
        override_rejected,
        guard,
    };
    if fields.stream {
        Ok(stream_response(context, handle).into_response())
    } else {
        wait_response(context, handle, format).await
    }
}

struct JobContext {
    app: SharedState,
    token_id: Uuid,
    created_at: i64,
    started: Instant,
    notices: Vec<String>,
    override_rejected: bool,
    guard: ActiveJobGuard,
}

impl JobContext {
    fn finish(&self, job: &Arc<Job>, outcome: &Result<FinalTranscript, String>) {
        let total_ms = self.started.elapsed().as_millis() as u64;
        let record = JobRecord {
            id: job.id,
            api_token_id: self.token_id,
            created_at: self.created_at,
            status: if outcome.is_ok() { "done" } else { "failed" },
            model: job.model.as_ref().map(|m| m.name.clone()),
            language: outcome.as_ref().ok().map(|t| t.language.clone()),
            audio_ms: job.audio_ms,
            words: outcome.as_ref().ok().map(|t| t.words as u64),
            parts: job.parts.len(),
            queue_ms: job.queue_time().map(|d| d.as_millis() as u64),
            total_ms,
            override_rejected: self.override_rejected,
            error: outcome.as_ref().err().cloned(),
        };
        match outcome {
            Ok(t) => info!(job = %job.id, words = t.words, total_ms, language = %t.language, "job finished"),
            Err(e) => warn!(job = %job.id, error = %e, "job failed"),
        }
        if let Err(e) = self.app.db.record_job(&record) {
            warn!("storing job statistics failed: {e:#}");
        }
    }
}

fn assemble(job: &Job, results: Vec<TranscriptResult>, notices: Vec<String>) -> FinalTranscript {
    // The language most parts agree on.
    let mut languages: Vec<&str> = results
        .iter()
        .map(|r| r.language.as_str())
        .filter(|l| !l.is_empty())
        .collect();
    languages.sort_unstable();
    let language = languages
        .chunk_by(|a, b| a == b)
        .max_by_key(|group| group.len())
        .and_then(|group| group.first())
        .map_or_else(String::new, |l| (*l).to_string());
    let segments = results.into_iter().flat_map(|r| r.segments).collect();
    FinalTranscript::new(
        job.id,
        segments,
        language,
        job.model.as_ref().map(|m| m.name.clone()),
        job.audio_ms,
        job.parts.len(),
        notices,
    )
}

async fn wait_response(context: JobContext, handle: JobHandle, format: OutputFormat) -> Result<Response, ApiError> {
    let JobHandle { job, outcome, .. } = handle;
    // If the caller disconnects, this future is dropped and the job is cancelled.
    let cancel_on_drop = job.cancel.clone().drop_guard();
    let outcome = outcome
        .await
        .unwrap_or_else(|_| Err("the job was cancelled".to_string()));
    cancel_on_drop.disarm();
    let transcript = outcome.map(|results| assemble(&job, results, context.notices.clone()));
    context.finish(&job, &transcript);
    drop(context.guard);

    let transcript = transcript.map_err(|e| ApiError::new(axum::http::StatusCode::BAD_GATEWAY, e))?;
    let mut response = (
        [(header::CONTENT_TYPE, format.content_type())],
        transcript.render(format),
    )
        .into_response();
    if format != OutputFormat::Json {
        for notice in &transcript.notices {
            if let Ok(value) = HeaderValue::from_str(notice) {
                response.headers_mut().append("x-verbarelay-notice", value);
            }
        }
    }
    Ok(response)
}

fn sse_event(name: &str, data: serde_json::Value) -> Event {
    Event::default().event(name).data(data.to_string())
}

fn stream_response(
    context: JobContext,
    handle: JobHandle,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel::<Event>(64);
    let JobHandle {
        job,
        mut events,
        mut outcome,
    } = handle;

    tokio::spawn(async move {
        let send = |event: Event| {
            let tx = tx.clone();
            async move { tx.send(event).await.is_ok() }
        };
        let queued = json!({
            "job_id": job.id,
            "duration_ms": job.audio_ms,
            "parts": job.parts.len(),
            "notices": context.notices,
        });
        let mut alive = send(sse_event("queued", queued)).await;
        for notice in &context.notices {
            alive &= send(sse_event("notice", json!({ "message": notice }))).await;
        }

        let forward = |event: JobEvent| match event {
            JobEvent::Assigned { part, peer } => sse_event("assigned", json!({ "part": part, "peer": peer })),
            JobEvent::Segment { part, segment } => sse_event(
                "segment",
                json!({ "part": part, "start_ms": segment.start_ms, "end_ms": segment.end_ms, "text": segment.text }),
            ),
        };

        let result = loop {
            if !alive {
                break Err("the client disconnected".to_string());
            }
            tokio::select! {
                Some(event) = events.recv() => alive = send(forward(event)).await,
                result = &mut outcome => break result.unwrap_or_else(|_| Err("the job was cancelled".to_string())),
                () = tx.closed() => alive = false,
            }
        };
        if !alive {
            job.cancel.cancel();
        }
        // Segments that arrived together with the final result.
        while let Ok(event) = events.try_recv() {
            send(forward(event)).await;
        }

        let transcript = result.map(|results| assemble(&job, results, context.notices.clone()));
        context.finish(&job, &transcript);
        match &transcript {
            Ok(t) => send(sse_event("done", serde_json::to_value(t).unwrap_or_default())).await,
            Err(e) => send(sse_event("error", json!({ "error": e }))).await,
        };
        drop(context.guard);
    });

    Sse::new(ReceiverStream::new(rx).map(Ok)).keep_alive(KeepAlive::default())
}
