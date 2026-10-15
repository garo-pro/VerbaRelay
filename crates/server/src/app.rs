//! Shared application state, router and error type.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;
use tower_http::trace::TraceLayer;
use uuid::Uuid;
use verbarelay_protocol::{PEER_CONNECT_PATH, PEER_ENROLL_PATH};

use crate::config::ServerConfig;
use crate::db::Db;
use crate::rate_limit::RateLimiter;
use crate::scheduler::Scheduler;
use crate::{api, peers};

/// Everything the request handlers need.
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub db: Arc<Db>,
    pub scheduler: Arc<Scheduler>,
    pub api_limiter: RateLimiter<Uuid>,
    pub enroll_limiter: RateLimiter<IpAddr>,
    pub active_jobs: Mutex<HashMap<Uuid, u32>>,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn new(config: Arc<ServerConfig>, db: Arc<Db>, scheduler: Arc<Scheduler>) -> SharedState {
        let rl = &config.rate_limit;
        Arc::new(Self {
            api_limiter: RateLimiter::new(rl.requests_per_minute, rl.burst),
            enroll_limiter: RateLimiter::new(rl.enroll_attempts_per_minute, rl.enroll_attempts_per_minute),
            active_jobs: Mutex::new(HashMap::new()),
            config,
            db,
            scheduler,
        })
    }

    /// The client address, honouring X-Forwarded-For only when configured.
    pub fn client_ip(&self, addr: SocketAddr, headers: &HeaderMap) -> IpAddr {
        if self.config.server.trust_forwarded_for
            && let Some(ip) = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
        addr.ip()
    }
}

/// Builds the HTTP router.
pub fn router(state: SharedState) -> axum::Router {
    let max_body = usize::try_from(state.config.server.max_upload_mb * 1024 * 1024).unwrap_or(usize::MAX);
    axum::Router::new()
        .route("/api/v1/health", get(api::health))
        .route("/api/v1/status", get(api::status))
        .route(
            "/api/v1/transcriptions",
            post(api::transcribe).layer(DefaultBodyLimit::max(max_body)),
        )
        .route(PEER_ENROLL_PATH, post(peers::enroll))
        .route(PEER_CONNECT_PATH, get(peers::connect))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// An error answered as `{"error": "..."}`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub retry_after_secs: Option<u64>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "missing or invalid token")
    }

    pub fn too_many(message: impl Into<String>, retry_after: std::time::Duration) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: message.into(),
            retry_after_secs: Some(retry_after.as_secs().max(1)),
        }
    }

    pub fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!("internal error: {error}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(json!({ "error": self.message }))).into_response();
        if let Some(secs) = self.retry_after_secs
            && let Ok(value) = HeaderValue::from_str(&secs.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}
