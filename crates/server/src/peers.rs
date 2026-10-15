//! Peer enrollment and the peer WebSocket.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;
use verbarelay_protocol::crypto::decode_key;
use verbarelay_protocol::messages::{EnrollRequest, EnrollResponse, PeerMessage, ServerMessage};
use verbarelay_protocol::{PEER_TOKEN_PREFIX, PROTOCOL_VERSION};

use crate::app::{ApiError, SharedState};
use crate::auth;
use crate::db::PeerRow;
use crate::scheduler::{OfferAnswer, Outbound, PeerConn};

/// Peers that send nothing (not even a pong) for this long are dropped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const TOUCH_INTERVAL: Duration = Duration::from_secs(300);
const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

/// `POST /api/v1/peers/enroll`: exchanges the master password for a peer token.
pub async fn enroll(
    State(app): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<EnrollRequest>,
) -> Result<Json<EnrollResponse>, ApiError> {
    let ip = app.client_ip(addr, &headers);
    if let Err(wait) = app.enroll_limiter.check(&ip) {
        warn!(%ip, "enrollment rate limit hit");
        return Err(ApiError::too_many("too many enrollment attempts", wait));
    }

    let enrollment = app.config.enrollment.clone();
    let attempt = request.master_password.clone();
    let valid = tokio::task::spawn_blocking(move || auth::verify_master_password(&enrollment, &attempt))
        .await
        .map_err(ApiError::internal)?;
    if !valid {
        warn!(%ip, name = %request.name, "enrollment with a wrong master password");
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "wrong master password"));
    }

    let name = request.name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "the device name must be 1 to 64 printable characters",
        ));
    }
    decode_key(&request.public_key).map_err(|_| ApiError::bad_request("invalid public key"))?;

    let peer_id = Uuid::new_v4();
    let token = auth::generate_token(PEER_TOKEN_PREFIX);
    app.db
        .insert_peer(
            peer_id,
            name,
            request.public_key.trim(),
            &auth::hash_token(&token),
            &request.client_version,
        )
        .map_err(ApiError::internal)?;
    info!(peer = %peer_id, %name, %ip, version = %request.client_version, "peer enrolled");

    Ok(Json(EnrollResponse {
        peer_id,
        token,
        token_ttl_days: app.config.enrollment.peer_token_ttl_days,
    }))
}

/// `GET /api/v1/peers/connect`: the peer WebSocket (authenticated with the peer token).
pub async fn connect(
    State(app): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let token = auth::bearer(headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()))
        .ok_or_else(ApiError::unauthorized)?;
    let Some(peer) = app
        .db
        .peer_by_token_hash(&auth::hash_token(token))
        .map_err(ApiError::internal)?
    else {
        let ip = app.client_ip(addr, &headers);
        warn!(%ip, token = %auth::redact(token), "peer connection with an unknown or expired token");
        return Err(ApiError::unauthorized());
    };
    let public_key = decode_key(&peer.public_key).map_err(ApiError::internal)?;
    Ok(ws
        .max_message_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| async move { run_peer(app, peer, public_key, socket).await }))
}

async fn run_peer(app: SharedState, peer: PeerRow, public_key: [u8; 32], socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    // The first message must be a hello.
    let hello = match tokio::time::timeout(Duration::from_secs(15), stream.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<PeerMessage>(&text).ok(),
        _ => None,
    };
    let Some(PeerMessage::Hello {
        protocol_version,
        client_version,
        device,
        models,
        busy,
    }) = hello
    else {
        debug!(peer = %peer.id, "connection closed before hello");
        return;
    };
    if protocol_version != PROTOCOL_VERSION {
        warn!(peer = %peer.name, protocol_version, "incompatible protocol version");
        let _ = sink
            .send(Message::Close(Some(CloseFrame {
                code: 4000,
                reason: format!("protocol version {PROTOCOL_VERSION} required").into(),
            })))
            .await;
        return;
    }

    let backend = device.backend.to_string();
    let device_json = serde_json::to_string(&device).unwrap_or_default();
    if let Err(e) = app
        .db
        .update_peer_hello(peer.id, &client_version, &device_json, &backend)
    {
        warn!("storing peer hello failed: {e:#}");
    }
    let (stored_score, stored_model) = if peer.backend.as_deref() == Some(backend.as_str()) {
        (peer.score, peer.score_model.clone())
    } else {
        (None, None)
    };

    let (tx, mut rx) = mpsc::channel::<Outbound>(16);
    let conn = Arc::new(PeerConn::new(peer.id, peer.name.clone(), public_key, tx.clone()));
    let accelerator = device.accelerator.clone();
    let benchmark = app
        .scheduler
        .add_peer(conn.clone(), device, models, busy, stored_score, stored_model);
    let welcome = ServerMessage::Welcome {
        peer_id: peer.id,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        policy: app.scheduler.policy(),
        benchmark,
    };
    if send_json(&mut sink, &welcome).await.is_err() {
        app.scheduler.remove_peer(&conn);
        return;
    }
    let session = app.db.open_session(peer.id).ok();
    info!(peer = %peer.name, %accelerator, %backend, version = %client_version, "peer connected");

    // Writer: outbound messages and keep-alive pings.
    let writer_conn = conn.clone();
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        loop {
            tokio::select! {
                out = rx.recv() => {
                    let result = match out {
                        Some(Outbound::Json(msg)) => send_json(&mut sink, &msg).await,
                        Some(Outbound::Binary(bytes)) => sink.send(Message::Binary(bytes.into())).await,
                        Some(Outbound::Close) | None => {
                            let _ = sink.close().await;
                            break;
                        }
                    };
                    if result.is_err() {
                        break;
                    }
                }
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
                () = writer_conn.closed.cancelled() => {
                    let _ = sink.close().await;
                    break;
                }
            }
        }
        writer_conn.closed.cancel();
    });

    // Keeps last_seen fresh and notices revocation.
    let touch_app = app.clone();
    let touch_conn = conn.clone();
    let toucher = tokio::spawn(async move {
        let mut tick = tokio::time::interval(TOUCH_INTERVAL);
        loop {
            tick.tick().await;
            match touch_app.db.touch_peer(touch_conn.id) {
                Ok(true) => {}
                Ok(false) => {
                    info!(peer = %touch_conn.name, "peer was revoked or expired; disconnecting");
                    let _ = touch_conn.tx.send(Outbound::Json(ServerMessage::Revoked)).await;
                    let _ = touch_conn.tx.send(Outbound::Close).await;
                    break;
                }
                Err(e) => warn!("updating last_seen failed: {e:#}"),
            }
        }
    });

    // Reader.
    loop {
        let next = tokio::select! {
            next = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => next,
            () = conn.closed.cancelled() => break,
        };
        let message = match next {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(e))) => {
                debug!(peer = %peer.name, "websocket error: {e}");
                break;
            }
            Ok(None) => break,
            Err(_) => {
                info!(peer = %peer.name, "peer timed out");
                break;
            }
        };
        match message {
            Message::Text(text) => match serde_json::from_str::<PeerMessage>(&text) {
                Ok(msg) => handle_message(&app, &conn, msg),
                Err(e) => warn!(peer = %peer.name, "invalid message: {e}"),
            },
            Message::Close(_) => break,
            // Pings are answered automatically; any frame counts as a sign of life.
            _ => {}
        }
    }

    conn.closed.cancel();
    toucher.abort();
    let _ = writer.await;
    app.scheduler.remove_peer(&conn);
    if let Some(session) = session {
        let _ = app.db.close_session(session);
    }
    let _ = app.db.touch_peer(peer.id);
    info!(peer = %peer.name, "peer disconnected");
}

fn handle_message(app: &SharedState, conn: &Arc<PeerConn>, msg: PeerMessage) {
    match msg {
        PeerMessage::Hello { .. } => warn!(peer = %conn.name, "duplicate hello ignored"),
        PeerMessage::Models { models } => {
            debug!(peer = %conn.name, ?models, "models updated");
            app.scheduler.update_models(conn.id, models);
        }
        PeerMessage::Benchmark { result } => app.scheduler.update_benchmark(conn.id, result),
        PeerMessage::OfferReply { job_id, accept, reason } => {
            conn.resolve_offer(job_id, OfferAnswer { accept, reason });
        }
        PeerMessage::Segment { job_id, seq, sealed } => {
            if let Err(e) = conn.handle_segment(job_id, seq, &sealed) {
                warn!(peer = %conn.name, %job_id, "rejected segment: {e}");
            }
        }
        PeerMessage::JobFinished { job_id, seq, sealed } => {
            if let Err(e) = conn.handle_finished(job_id, seq, &sealed) {
                warn!(peer = %conn.name, %job_id, "rejected result: {e}");
                conn.handle_failed(job_id, format!("invalid result: {e}"), true);
            }
        }
        PeerMessage::JobFailed {
            job_id,
            error,
            retryable,
        } => conn.handle_failed(job_id, error, retryable),
    }
}

async fn send_json<S>(sink: &mut S, msg: &ServerMessage) -> Result<(), axum::Error>
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    let text = serde_json::to_string(msg).expect("server messages serialize");
    sink.send(Message::Text(text.into())).await
}
