//! Peer index and job dispatching.
//!
//! The scheduler keeps every connected peer with its score and state. Incoming
//! jobs are split into work items (one per part) and queued. The dispatcher hands
//! each item to the free peer with the highest transcription score:
//!
//! 1. it sends an offer and waits for the peer to accept ("are you there?"),
//! 2. encrypts the audio for exactly this peer and streams it,
//! 3. forwards live segments and collects the final result,
//! 4. retries on another peer if the peer declines, times out or disconnects.
//!
//! A peer works on at most one item at a time.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;
use verbarelay_protocol::crypto::{self, SessionKeys};
use verbarelay_protocol::frames::{AUDIO_CHUNK_SIZE, AudioChunk, pcm_to_bytes};
use verbarelay_protocol::messages::{
    BenchmarkRequest, BenchmarkResult, DECLINE_BUSY, DECLINE_MODEL_MISSING, DeviceInfo, ModelPolicy, ServerMessage,
};
use verbarelay_protocol::models::ModelSpec;
use verbarelay_protocol::transcript::{Segment, TranscriptResult};

use crate::config::ServerConfig;
use crate::db::{Db, PartRecord};

/// How long a peer that declined with "busy" is skipped.
const BUSY_COOLDOWN: Duration = Duration::from_secs(2);

/// Something to send to a peer's WebSocket.
#[derive(Debug)]
pub enum Outbound {
    Json(ServerMessage),
    Binary(Vec<u8>),
    Close,
}

/// A peer's answer to an offer.
#[derive(Debug, Clone)]
pub struct OfferAnswer {
    pub accept: bool,
    pub reason: Option<String>,
}

/// What a peer reports while working on an item.
#[derive(Debug)]
pub enum PartEvent {
    Segment(Segment),
    Finished(TranscriptResult),
    Failed { error: String, retryable: bool },
}

/// An item a peer is currently working on.
pub struct ActiveJob {
    keys: Arc<SessionKeys>,
    last_seq: Mutex<Option<u32>>,
    events: mpsc::UnboundedSender<PartEvent>,
}

/// One live connection of a peer.
pub struct PeerConn {
    pub id: Uuid,
    pub name: String,
    pub public_key: [u8; 32],
    pub tx: mpsc::Sender<Outbound>,
    pub closed: CancellationToken,
    pending_offers: Mutex<HashMap<Uuid, oneshot::Sender<OfferAnswer>>>,
    active: Mutex<HashMap<Uuid, Arc<ActiveJob>>>,
}

/// Errors while handling a peer's result messages.
#[derive(Debug, thiserror::Error)]
pub enum ResultError {
    #[error("unknown job")]
    UnknownJob,
    #[error("invalid encoding")]
    Encoding,
    #[error("sequence number replayed")]
    Replay,
    #[error(transparent)]
    Crypto(#[from] crypto::CryptoError),
    #[error("invalid payload: {0}")]
    Payload(#[from] serde_json::Error),
}

impl PeerConn {
    pub fn new(id: Uuid, name: String, public_key: [u8; 32], tx: mpsc::Sender<Outbound>) -> Self {
        Self {
            id,
            name,
            public_key,
            tx,
            closed: CancellationToken::new(),
            pending_offers: Mutex::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
        }
    }

    /// Sends an offer and waits for the answer.
    async fn offer(&self, msg: ServerMessage, job_id: Uuid, timeout: Duration) -> Option<OfferAnswer> {
        let (tx, rx) = oneshot::channel();
        self.pending_offers.lock().unwrap().insert(job_id, tx);
        let answer = async {
            self.tx.send(Outbound::Json(msg)).await.ok()?;
            tokio::select! {
                answer = rx => answer.ok(),
                () = self.closed.cancelled() => None,
                () = tokio::time::sleep(timeout) => None,
            }
        }
        .await;
        self.pending_offers.lock().unwrap().remove(&job_id);
        answer
    }

    /// Called by the WebSocket handler for an `offer_reply`.
    pub fn resolve_offer(&self, job_id: Uuid, answer: OfferAnswer) {
        if let Some(tx) = self.pending_offers.lock().unwrap().remove(&job_id) {
            let _ = tx.send(answer);
        }
    }

    fn register(&self, job_id: Uuid, job: Arc<ActiveJob>) {
        self.active.lock().unwrap().insert(job_id, job);
    }

    fn unregister(&self, job_id: Uuid) {
        self.active.lock().unwrap().remove(&job_id);
    }

    fn active_job(&self, job_id: Uuid) -> Result<Arc<ActiveJob>, ResultError> {
        self.active
            .lock()
            .unwrap()
            .get(&job_id)
            .cloned()
            .ok_or(ResultError::UnknownJob)
    }

    fn open(&self, job_id: Uuid, seq: u32, last: bool, sealed: &str) -> Result<(Arc<ActiveJob>, Vec<u8>), ResultError> {
        let job = self.active_job(job_id)?;
        {
            let mut last_seq = job.last_seq.lock().unwrap();
            if last_seq.is_some_and(|l| seq <= l) {
                return Err(ResultError::Replay);
            }
            *last_seq = Some(seq);
        }
        let bytes = verbarelay_protocol::messages::decode_sealed(sealed).ok_or(ResultError::Encoding)?;
        let plain = job.keys.open_result(seq, last, &bytes)?;
        Ok((job, plain))
    }

    /// Called by the WebSocket handler for a live segment.
    pub fn handle_segment(&self, job_id: Uuid, seq: u32, sealed: &str) -> Result<(), ResultError> {
        let (job, plain) = self.open(job_id, seq, false, sealed)?;
        let segment: Segment = serde_json::from_slice(&plain)?;
        let _ = job.events.send(PartEvent::Segment(segment));
        Ok(())
    }

    /// Called by the WebSocket handler for the final result.
    pub fn handle_finished(&self, job_id: Uuid, seq: u32, sealed: &str) -> Result<(), ResultError> {
        let (job, plain) = self.open(job_id, seq, true, sealed)?;
        let result: TranscriptResult = serde_json::from_slice(&plain)?;
        let _ = job.events.send(PartEvent::Finished(result));
        Ok(())
    }

    /// Called by the WebSocket handler when the peer reports a failure.
    pub fn handle_failed(&self, job_id: Uuid, error: String, retryable: bool) {
        if let Ok(job) = self.active_job(job_id) {
            let _ = job.events.send(PartEvent::Failed { error, retryable });
        }
    }
}

/// Live events of a job, consumed by the API handler (for streaming).
#[derive(Debug, Clone)]
pub enum JobEvent {
    Assigned { part: usize, peer: String },
    Segment { part: usize, segment: Segment },
}

/// Where one part of a job lies in the recording.
#[derive(Debug, Clone, Copy)]
pub struct PartRange {
    pub start_sample: usize,
    pub end_sample: usize,
}

impl PartRange {
    pub fn offset_ms(&self) -> u64 {
        self.start_sample as u64 * 1000 / u64::from(verbarelay_protocol::SAMPLE_RATE)
    }

    pub fn audio_ms(&self) -> u64 {
        (self.end_sample - self.start_sample) as u64 * 1000 / u64::from(verbarelay_protocol::SAMPLE_RATE)
    }
}

type Outcome = Result<Vec<TranscriptResult>, String>;

/// A transcription request.
pub struct Job {
    pub id: Uuid,
    pub model: Option<ModelSpec>,
    pub language: Option<String>,
    pub translate: bool,
    pub audio_ms: u64,
    pub parts: Vec<PartRange>,
    pub created: Instant,
    pub cancel: CancellationToken,
    pcm: Arc<Vec<i16>>,
    results: Mutex<Vec<Option<TranscriptResult>>>,
    events: mpsc::UnboundedSender<JobEvent>,
    outcome: Mutex<Option<oneshot::Sender<Outcome>>>,
    first_assigned: Mutex<Option<Instant>>,
}

/// Handles the caller keeps for a submitted job.
pub struct JobHandle {
    pub job: Arc<Job>,
    pub events: mpsc::UnboundedReceiver<JobEvent>,
    pub outcome: oneshot::Receiver<Outcome>,
}

impl Job {
    pub fn create(
        model: Option<ModelSpec>,
        language: Option<String>,
        translate: bool,
        pcm: Vec<i16>,
        split_points: &[usize],
    ) -> JobHandle {
        let mut bounds = vec![0];
        bounds.extend_from_slice(split_points);
        bounds.push(pcm.len());
        let parts: Vec<PartRange> = bounds
            .windows(2)
            .map(|w| PartRange {
                start_sample: w[0],
                end_sample: w[1],
            })
            .collect();
        let audio_ms = pcm.len() as u64 * 1000 / u64::from(verbarelay_protocol::SAMPLE_RATE);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let job = Arc::new(Self {
            id: Uuid::new_v4(),
            model,
            language,
            translate,
            audio_ms,
            results: Mutex::new(vec![None; parts.len()]),
            parts,
            created: Instant::now(),
            cancel: CancellationToken::new(),
            pcm: Arc::new(pcm),
            events: events_tx,
            outcome: Mutex::new(Some(outcome_tx)),
            first_assigned: Mutex::new(None),
        });
        JobHandle {
            job,
            events: events_rx,
            outcome: outcome_rx,
        }
    }

    pub fn is_done(&self) -> bool {
        self.cancel.is_cancelled() || self.outcome.lock().unwrap().is_none()
    }

    /// Time from submission until the first part was accepted by a peer.
    pub fn queue_time(&self) -> Option<Duration> {
        self.first_assigned
            .lock()
            .unwrap()
            .map(|t| t.duration_since(self.created))
    }

    fn mark_assigned(&self, part: usize, peer: &str) {
        self.first_assigned.lock().unwrap().get_or_insert_with(Instant::now);
        let _ = self.events.send(JobEvent::Assigned {
            part,
            peer: peer.to_string(),
        });
    }

    /// Moves a part's segment onto the timeline of the whole recording.
    fn place(&self, part: usize, segment: &mut Segment) {
        let range = self.parts[part];
        let length = range.audio_ms();
        // Whisper's last timestamp may overshoot the audio slightly.
        segment.start_ms = segment.start_ms.min(length) + range.offset_ms();
        segment.end_ms = segment.end_ms.min(length) + range.offset_ms();
    }

    fn emit_segment(&self, part: usize, mut segment: Segment) {
        self.place(part, &mut segment);
        let _ = self.events.send(JobEvent::Segment { part, segment });
    }

    fn complete_part(&self, part: usize, mut result: TranscriptResult) {
        for s in &mut result.segments {
            self.place(part, s);
        }
        let mut results = self.results.lock().unwrap();
        results[part] = Some(result);
        if results.iter().all(Option::is_some) {
            let all = results.iter_mut().map(|r| r.take().expect("checked above")).collect();
            if let Some(tx) = self.outcome.lock().unwrap().take() {
                let _ = tx.send(Ok(all));
            }
        }
    }

    /// Fails the whole job and stops its other parts.
    pub fn fail(&self, error: String) {
        if let Some(tx) = self.outcome.lock().unwrap().take() {
            let _ = tx.send(Err(error));
        }
        self.cancel.cancel();
    }
}

/// One part of a job waiting for or running on a peer.
struct WorkItem {
    key: Uuid,
    job: Arc<Job>,
    part: usize,
    tried: Mutex<HashSet<Uuid>>,
    attempts: AtomicU32,
    enqueued_at: Instant,
}

struct PeerSlot {
    conn: Arc<PeerConn>,
    device: DeviceInfo,
    score: Option<f64>,
    score_model: Option<String>,
    models: HashSet<String>,
    downloading: HashSet<String>,
    busy: bool,
    /// The peer said it is busy (benchmark, model loading); ask again after this.
    busy_until: Option<Instant>,
    assigned: Option<Uuid>,
}

impl PeerSlot {
    fn rank(&self) -> f64 {
        self.score.unwrap_or(0.0)
    }

    fn is_free(&self) -> bool {
        self.assigned.is_none()
            && !self.busy
            && self.busy_until.is_none_or(|t| Instant::now() >= t)
            && !self.conn.closed.is_cancelled()
    }

    fn has_model(&self, model: Option<&ModelSpec>) -> bool {
        match model {
            Some(m) => self.models.contains(&m.name),
            None => !self.models.is_empty(),
        }
    }
}

#[derive(Default)]
struct State {
    peers: HashMap<Uuid, PeerSlot>,
    queue: VecDeque<Arc<WorkItem>>,
}

/// A snapshot of a connected peer for status output.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerStatus {
    pub id: Uuid,
    pub name: String,
    pub accelerator: String,
    pub backend: String,
    pub score: Option<f64>,
    pub working: bool,
    pub models: Vec<String>,
}

/// The peer index and dispatcher.
pub struct Scheduler {
    config: Arc<ServerConfig>,
    db: Arc<Db>,
    state: Mutex<State>,
    wake: Notify,
}

impl Scheduler {
    pub fn new(config: Arc<ServerConfig>, db: Arc<Db>) -> Arc<Self> {
        Arc::new(Self {
            config,
            db,
            state: Mutex::new(State::default()),
            wake: Notify::new(),
        })
    }

    /// The model policy sent to peers.
    pub fn policy(&self) -> ModelPolicy {
        ModelPolicy {
            enforced: self
                .config
                .models
                .enforce
                .then(|| self.config.model(&self.config.models.default))
                .flatten(),
            allowed: self.config.allowed_models(),
        }
    }

    fn enforced_model_name(&self) -> Option<String> {
        self.config.models.enforce.then(|| self.config.models.default.clone())
    }

    /// Registers a peer after its hello. Replaces an older connection of the same peer.
    /// Returns the benchmark request to send, if the peer has no valid score.
    pub fn add_peer(
        &self,
        conn: Arc<PeerConn>,
        device: DeviceInfo,
        models: Vec<String>,
        busy: bool,
        stored_score: Option<f64>,
        stored_score_model: Option<String>,
    ) -> Option<BenchmarkRequest> {
        let enforced = self.enforced_model_name();
        let score_valid = match (&enforced, &stored_score_model) {
            (Some(e), Some(m)) => e == m,
            (None, Some(_)) => true,
            _ => false,
        } && stored_score.is_some();

        let slot = PeerSlot {
            conn: conn.clone(),
            device,
            score: score_valid.then_some(stored_score).flatten(),
            score_model: score_valid.then_some(stored_score_model).flatten(),
            models: models.into_iter().collect(),
            downloading: HashSet::new(),
            busy,
            busy_until: None,
            assigned: None,
        };
        let old = self.state.lock().unwrap().peers.insert(conn.id, slot);
        if let Some(old) = old {
            info!(peer = %conn.id, "replacing older connection of the same peer");
            old.conn.closed.cancel();
            let _ = old.conn.tx.try_send(Outbound::Close);
        }
        self.wake.notify_one();

        if score_valid {
            None
        } else {
            Some(BenchmarkRequest {
                model: enforced.and_then(|n| self.config.model(&n)),
            })
        }
    }

    /// Removes a peer when its connection ends (only if it is still the same connection).
    pub fn remove_peer(&self, conn: &Arc<PeerConn>) {
        let mut state = self.state.lock().unwrap();
        if state.peers.get(&conn.id).is_some_and(|s| Arc::ptr_eq(&s.conn, conn)) {
            state.peers.remove(&conn.id);
        }
        drop(state);
        self.wake.notify_one();
    }

    /// The peer reported a new set of models on disk.
    /// The benchmark request from the welcome stays pending on the peer until
    /// the model is there, so no new request is needed here.
    pub fn update_models(&self, peer_id: Uuid, models: Vec<String>) {
        {
            let mut state = self.state.lock().unwrap();
            let Some(slot) = state.peers.get_mut(&peer_id) else {
                return;
            };
            slot.models = models.into_iter().collect();
            let models = slot.models.clone();
            slot.downloading.retain(|m| !models.contains(m));
        }
        self.wake.notify_one();
    }

    /// The peer finished a benchmark: its speed becomes the score.
    pub fn update_benchmark(&self, peer_id: Uuid, result: BenchmarkResult) {
        if let Some(enforced) = self.enforced_model_name()
            && result.model != enforced
        {
            debug!(peer = %peer_id, model = %result.model, "ignoring benchmark for a model other than the enforced one");
            return;
        }
        let score = result.speed();
        {
            let mut state = self.state.lock().unwrap();
            let Some(slot) = state.peers.get_mut(&peer_id) else {
                return;
            };
            slot.score = Some(score);
            slot.score_model = Some(result.model.clone());
        }
        info!(peer = %peer_id, model = %result.model, backend = %result.backend, score = format!("{score:.1}"), "benchmark received");
        if let Err(e) = self.db.update_peer_score(peer_id, score, &result.model) {
            warn!("storing score failed: {e:#}");
        }
        self.wake.notify_one();
    }

    /// Number of free peers that could take a job with this model right now.
    pub fn idle_peers_for(&self, model: Option<&ModelSpec>) -> usize {
        self.state
            .lock()
            .unwrap()
            .peers
            .values()
            .filter(|s| s.is_free() && s.has_model(model))
            .count()
    }

    /// Number of connected peers.
    pub fn peer_count(&self) -> usize {
        self.state.lock().unwrap().peers.len()
    }

    /// Status of all connected peers.
    pub fn peer_statuses(&self) -> Vec<PeerStatus> {
        let state = self.state.lock().unwrap();
        let mut list: Vec<PeerStatus> = state
            .peers
            .values()
            .map(|s| PeerStatus {
                id: s.conn.id,
                name: s.conn.name.clone(),
                accelerator: s.device.accelerator.clone(),
                backend: s.device.backend.to_string(),
                score: s.score,
                working: s.assigned.is_some(),
                models: s.models.iter().cloned().collect(),
            })
            .collect();
        list.sort_by(|a, b| b.score.unwrap_or(0.0).total_cmp(&a.score.unwrap_or(0.0)));
        list
    }

    /// Queues all parts of a job.
    pub fn submit(&self, job: Arc<Job>) {
        let mut state = self.state.lock().unwrap();
        for part in 0..job.parts.len() {
            state.queue.push_back(Arc::new(WorkItem {
                key: Uuid::new_v4(),
                job: job.clone(),
                part,
                tried: Mutex::new(HashSet::new()),
                attempts: AtomicU32::new(0),
                enqueued_at: Instant::now(),
            }));
        }
        drop(state);
        self.wake.notify_one();
    }

    /// Runs the dispatcher forever.
    pub async fn run(self: Arc<Self>) {
        loop {
            self.dispatch_once();
            tokio::select! {
                () = self.wake.notified() => {}
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    fn dispatch_once(self: &Arc<Self>) {
        let queue_timeout = Duration::from_secs(self.config.scheduling.queue_timeout_secs);
        let mut state = self.state.lock().unwrap();
        let mut index = 0;
        while index < state.queue.len() {
            let item = state.queue[index].clone();
            if item.job.is_done() {
                state.queue.remove(index);
                continue;
            }
            if item.enqueued_at.elapsed() > queue_timeout {
                state.queue.remove(index);
                let connected = state.peers.len();
                warn!(job = %item.job.id, part = item.part, connected, "no peer became available in time");
                item.job.fail(format!(
                    "no peer was able to take the job within {} seconds ({connected} peers connected)",
                    queue_timeout.as_secs()
                ));
                continue;
            }
            if let Some(peer_id) = Self::pick_peer(&state, &item) {
                let slot = state.peers.get_mut(&peer_id).expect("picked peer exists");
                slot.assigned = Some(item.key);
                let conn = slot.conn.clone();
                state.queue.remove(index);
                let this = self.clone();
                tokio::spawn(async move { this.run_item(item, conn).await });
                continue;
            }
            index += 1;
        }
    }

    fn pick_peer(state: &State, item: &WorkItem) -> Option<Uuid> {
        let model = item.job.model.as_ref();
        let tried = item.tried.lock().unwrap();
        let best = state
            .peers
            .values()
            .filter(|s| s.is_free() && s.has_model(model) && !tried.contains(&s.conn.id))
            .max_by(|a, b| a.rank().total_cmp(&b.rank()));
        if let Some(best) = best {
            return Some(best.conn.id);
        }
        // Nobody has the model. If no connected peer has it at all, ask the best free
        // peer anyway: it declines with "model_missing" and starts the download.
        let model = model?;
        if state
            .peers
            .values()
            .any(|s| s.models.contains(&model.name) || s.downloading.contains(&model.name))
        {
            return None;
        }
        state
            .peers
            .values()
            .filter(|s| s.is_free() && !tried.contains(&s.conn.id))
            .max_by(|a, b| a.rank().total_cmp(&b.rank()))
            .map(|s| s.conn.id)
    }

    fn release(&self, peer_id: Uuid, item: &WorkItem) {
        let mut state = self.state.lock().unwrap();
        if let Some(slot) = state.peers.get_mut(&peer_id)
            && slot.assigned == Some(item.key)
        {
            slot.assigned = None;
        }
        drop(state);
        self.wake.notify_one();
    }

    fn requeue(&self, item: Arc<WorkItem>) {
        self.state.lock().unwrap().queue.push_front(item);
        self.wake.notify_one();
    }

    fn mark_busy(&self, peer_id: Uuid) {
        if let Some(slot) = self.state.lock().unwrap().peers.get_mut(&peer_id) {
            slot.busy_until = Some(Instant::now() + BUSY_COOLDOWN);
        }
    }

    fn mark_downloading(&self, peer_id: Uuid, model: &str) {
        if let Some(slot) = self.state.lock().unwrap().peers.get_mut(&peer_id) {
            slot.downloading.insert(model.to_string());
        }
    }

    fn update_score_from_job(&self, peer_id: Uuid, model: Option<&str>, audio_ms: u64, processing_ms: u64) {
        if audio_ms < 5_000 || processing_ms == 0 {
            return; // Too short to say anything about speed.
        }
        let observed = audio_ms as f64 / processing_ms as f64;
        let enforced = self.enforced_model_name();
        let updated = {
            let mut state = self.state.lock().unwrap();
            let Some(slot) = state.peers.get_mut(&peer_id) else {
                return;
            };
            // Only learn from jobs that used the model the score refers to.
            let same_model = match (&enforced, model) {
                (Some(e), Some(m)) => e == m,
                (None, _) => true,
                _ => false,
            };
            if !same_model {
                return;
            }
            let score = slot.score.map_or(observed, |old| 0.8 * old + 0.2 * observed);
            slot.score = Some(score);
            slot.score_model.clone().map(|m| (score, m))
        };
        if let Some((score, model)) = updated
            && let Err(e) = self.db.update_peer_score(peer_id, score, &model)
        {
            warn!("storing score failed: {e:#}");
        }
    }

    async fn run_item(self: Arc<Self>, item: Arc<WorkItem>, conn: Arc<PeerConn>) {
        let job = item.job.clone();
        let range = job.parts[item.part];
        let wire_id = Uuid::new_v4();
        let mut record = PartRecord {
            id: wire_id,
            job_id: job.id,
            peer_id: Some(conn.id),
            status: "failed",
            audio_ms: range.audio_ms(),
            offer_ms: None,
            transfer_ms: None,
            processing_ms: None,
            first_segment_ms: None,
            error: None,
        };

        // 1. Offer: "are you there and free?"
        let offer_started = Instant::now();
        let offer = ServerMessage::Offer {
            job_id: wire_id,
            model: job.model.clone(),
            language: job.language.clone(),
            translate: job.translate,
            audio_ms: range.audio_ms(),
        };
        let timeout = Duration::from_secs(self.config.scheduling.offer_timeout_secs);
        let answer = conn.offer(offer, wire_id, timeout).await;
        record.offer_ms = Some(offer_started.elapsed().as_millis() as u64);

        match answer {
            Some(OfferAnswer { accept: true, .. }) => {}
            Some(OfferAnswer { reason, .. }) => {
                let reason = reason.unwrap_or_default();
                debug!(peer = %conn.name, %reason, "offer declined");
                if reason == DECLINE_MODEL_MISSING {
                    if let Some(model) = &job.model {
                        self.mark_downloading(conn.id, &model.name);
                    }
                } else if reason == DECLINE_BUSY {
                    // Busy is temporary (benchmark, another server's view): ask again soon.
                    self.mark_busy(conn.id);
                } else {
                    item.tried.lock().unwrap().insert(conn.id);
                }
                record.status = "declined";
                record.error = Some(reason);
                self.finish_record(&record);
                self.release(conn.id, &item);
                self.requeue(item);
                return;
            }
            None => {
                info!(peer = %conn.name, "peer did not answer the offer in time");
                item.tried.lock().unwrap().insert(conn.id);
                record.status = "offer_timeout";
                self.finish_record(&record);
                self.release(conn.id, &item);
                self.requeue(item);
                return;
            }
        }
        if job.is_done() {
            let _ = conn
                .tx
                .try_send(Outbound::Json(ServerMessage::Cancel { job_id: wire_id }));
            self.release(conn.id, &item);
            return;
        }
        job.mark_assigned(item.part, &conn.name);
        info!(job = %job.id, part = item.part, peer = %conn.name, audio_ms = range.audio_ms(), "job part assigned");

        // 2. Encrypt the audio for this peer and stream it.
        let outcome = self.transfer_and_wait(&item, &conn, wire_id, range, &mut record).await;

        conn.unregister(wire_id);
        match outcome {
            Ok(result) => {
                record.status = "done";
                record.processing_ms = Some(result.processing_ms);
                record.first_segment_ms = result.first_segment_ms;
                self.finish_record(&record);
                self.update_score_from_job(
                    conn.id,
                    job.model.as_ref().map(|m| m.name.as_str()),
                    range.audio_ms(),
                    result.processing_ms,
                );
                job.complete_part(item.part, result);
                self.release(conn.id, &item);
            }
            Err(ItemFailure::Cancelled) => {
                record.status = "cancelled";
                self.finish_record(&record);
                self.release(conn.id, &item);
            }
            Err(ItemFailure::Failed { error, retryable }) => {
                warn!(job = %job.id, part = item.part, peer = %conn.name, %error, retryable, "job part failed");
                record.error = Some(error.clone());
                self.finish_record(&record);
                item.tried.lock().unwrap().insert(conn.id);
                self.release(conn.id, &item);
                let attempts = item.attempts.fetch_add(1, Ordering::SeqCst) + 1;
                if retryable && attempts < self.config.scheduling.max_attempts {
                    self.requeue(item);
                } else {
                    job.fail(format!("transcription failed: {error}"));
                }
            }
        }
    }

    async fn transfer_and_wait(
        &self,
        item: &WorkItem,
        conn: &Arc<PeerConn>,
        wire_id: Uuid,
        range: PartRange,
        record: &mut PartRecord,
    ) -> Result<TranscriptResult, ItemFailure> {
        let job = &item.job;
        let (ephemeral, keys) = crypto::create_session(conn.public_key, wire_id).map_err(|e| ItemFailure::Failed {
            error: e.to_string(),
            retryable: true,
        })?;
        let keys = Arc::new(keys);
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        conn.register(
            wire_id,
            Arc::new(ActiveJob {
                keys: keys.clone(),
                last_seq: Mutex::new(None),
                events: events_tx,
            }),
        );

        let transfer_started = Instant::now();
        let bytes = zeroize::Zeroizing::new(pcm_to_bytes(&job.pcm[range.start_sample..range.end_sample]));
        let chunks: Vec<&[u8]> = bytes.chunks(AUDIO_CHUNK_SIZE).collect();
        let chunk_count = chunks.len() as u32;
        let header = ServerMessage::Audio {
            job_id: wire_id,
            ephemeral_public_key: crypto::encode_key(&ephemeral),
            chunk_count,
            sample_count: (range.end_sample - range.start_sample) as u64,
        };
        let disconnected = || ItemFailure::Failed {
            error: "peer disconnected".into(),
            retryable: true,
        };
        conn.tx.send(Outbound::Json(header)).await.map_err(|_| disconnected())?;
        for (seq, chunk) in chunks.iter().enumerate() {
            let last = seq + 1 == chunks.len();
            let ciphertext = keys
                .seal_audio(seq as u32, last, chunk)
                .map_err(|e| ItemFailure::Failed {
                    error: e.to_string(),
                    retryable: true,
                })?;
            let frame = AudioChunk {
                job_id: wire_id,
                seq: seq as u32,
                last,
                ciphertext,
            }
            .encode();
            tokio::select! {
                sent = conn.tx.send(Outbound::Binary(frame)) => sent.map_err(|_| disconnected())?,
                () = job.cancel.cancelled() => return Err(ItemFailure::Cancelled),
            }
        }
        drop(bytes);
        record.transfer_ms = Some(transfer_started.elapsed().as_millis() as u64);

        // 3. Wait for live segments and the result.
        let limit = Duration::from_secs_f64(
            (range.audio_ms() as f64 / 1000.0 * self.config.scheduling.job_timeout_factor).max(120.0),
        );
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            tokio::select! {
                event = events_rx.recv() => match event {
                    Some(PartEvent::Segment(segment)) => job.emit_segment(item.part, segment),
                    Some(PartEvent::Finished(result)) => return Ok(result),
                    Some(PartEvent::Failed { error, retryable }) => return Err(ItemFailure::Failed { error, retryable }),
                    None => return Err(disconnected()),
                },
                () = conn.closed.cancelled() => return Err(disconnected()),
                () = job.cancel.cancelled() => {
                    let _ = conn.tx.try_send(Outbound::Json(ServerMessage::Cancel { job_id: wire_id }));
                    return Err(ItemFailure::Cancelled);
                }
                () = tokio::time::sleep_until(deadline) => {
                    let _ = conn.tx.try_send(Outbound::Json(ServerMessage::Cancel { job_id: wire_id }));
                    return Err(ItemFailure::Failed {
                        error: format!("peer did not finish within {} seconds", limit.as_secs()),
                        retryable: true,
                    });
                }
            }
        }
    }

    fn finish_record(&self, record: &PartRecord) {
        if let Err(e) = self.db.record_part(record) {
            warn!("storing job part statistics failed: {e:#}");
        }
    }
}

enum ItemFailure {
    Cancelled,
    Failed { error: String, retryable: bool },
}

#[cfg(test)]
mod tests {
    use super::*;
    use verbarelay_protocol::crypto::PeerKeypair;
    use verbarelay_protocol::frames::bytes_to_pcm;
    use verbarelay_protocol::messages::Backend;

    fn test_config() -> Arc<ServerConfig> {
        let mut config: ServerConfig = toml::from_str(crate::config::EXAMPLE_CONFIG).unwrap();
        config.models.default = "tiny".into();
        config.scheduling.offer_timeout_secs = 1;
        Arc::new(config)
    }

    fn device() -> DeviceInfo {
        DeviceInfo {
            hostname: "test".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            cpu_name: "cpu".into(),
            cpu_threads: 8,
            memory_total: 0,
            gpus: vec![],
            backend: Backend::Cpu,
            accelerator: "cpu".into(),
        }
    }

    /// A fake peer that answers offers and returns the number of samples it received as text.
    fn spawn_fake_peer(
        scheduler: &Arc<Scheduler>,
        score: f64,
        accept: bool,
    ) -> (Arc<PeerConn>, tokio::task::JoinHandle<u32>) {
        let keys = PeerKeypair::generate();
        let (tx, mut rx) = mpsc::channel(64);
        let conn = Arc::new(PeerConn::new(
            Uuid::new_v4(),
            format!("peer-{score}"),
            keys.public_bytes(),
            tx,
        ));
        scheduler.add_peer(
            conn.clone(),
            device(),
            vec!["tiny".into()],
            false,
            Some(score),
            Some("tiny".into()),
        );
        let peer = conn.clone();
        let handle = tokio::spawn(async move {
            let mut jobs_done = 0;
            let mut session: Option<(Uuid, SessionKeys, Vec<u8>)> = None;
            while let Some(msg) = rx.recv().await {
                match msg {
                    Outbound::Json(ServerMessage::Offer { job_id, .. }) => {
                        peer.resolve_offer(
                            job_id,
                            OfferAnswer {
                                accept,
                                reason: (!accept).then(|| "busy".into()),
                            },
                        );
                    }
                    Outbound::Json(ServerMessage::Audio {
                        job_id,
                        ephemeral_public_key,
                        ..
                    }) => {
                        let eph = crypto::decode_key(&ephemeral_public_key).unwrap();
                        session = Some((job_id, keys.open_session(eph, job_id).unwrap(), Vec::new()));
                    }
                    Outbound::Binary(frame) => {
                        let chunk = AudioChunk::decode(&frame).unwrap();
                        let (job_id, keys, buf) = session.as_mut().unwrap();
                        buf.extend(keys.open_audio(chunk.seq, chunk.last, &chunk.ciphertext).unwrap());
                        if chunk.last {
                            let samples = bytes_to_pcm(buf).len();
                            let segment = Segment {
                                start_ms: 0,
                                end_ms: 1000,
                                text: format!("{samples}"),
                            };
                            let sealed = keys
                                .seal_result(0, false, &serde_json::to_vec(&segment).unwrap())
                                .unwrap();
                            peer.handle_segment(*job_id, 0, &verbarelay_protocol::messages::encode_sealed(&sealed))
                                .unwrap();
                            let result = TranscriptResult {
                                language: "en".into(),
                                segments: vec![segment],
                                processing_ms: 10,
                                first_segment_ms: Some(5),
                            };
                            let sealed = keys
                                .seal_result(1, true, &serde_json::to_vec(&result).unwrap())
                                .unwrap();
                            peer.handle_finished(*job_id, 1, &verbarelay_protocol::messages::encode_sealed(&sealed))
                                .unwrap();
                            jobs_done += 1;
                        }
                    }
                    Outbound::Close => break,
                    _ => {}
                }
            }
            jobs_done
        });
        (conn, handle)
    }

    #[tokio::test]
    async fn highest_score_gets_the_job_and_results_are_encrypted_end_to_end() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let scheduler = Scheduler::new(test_config(), db);
        tokio::spawn(scheduler.clone().run());

        let (slow, slow_handle) = spawn_fake_peer(&scheduler, 2.0, true);
        let (fast, fast_handle) = spawn_fake_peer(&scheduler, 20.0, true);

        let pcm = vec![1i16; 16_000 * 3];
        let handle = Job::create(verbarelay_protocol::models::find("tiny"), None, false, pcm, &[]);
        scheduler.submit(handle.job.clone());
        let results = tokio::time::timeout(Duration::from_secs(5), handle.outcome)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(results[0].segments[0].text, "48000");

        let _ = slow.tx.send(Outbound::Close).await;
        let _ = fast.tx.send(Outbound::Close).await;
        assert_eq!(fast_handle.await.unwrap(), 1);
        assert_eq!(slow_handle.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn declined_offers_move_to_the_next_peer_and_parts_get_offsets() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let scheduler = Scheduler::new(test_config(), db);
        tokio::spawn(scheduler.clone().run());

        let (_grumpy, _) = spawn_fake_peer(&scheduler, 50.0, false);
        let (_a, _) = spawn_fake_peer(&scheduler, 10.0, true);
        let (_b, _) = spawn_fake_peer(&scheduler, 9.0, true);

        let pcm = vec![1i16; 16_000 * 10];
        let mut handle = Job::create(
            verbarelay_protocol::models::find("tiny"),
            None,
            false,
            pcm,
            &[16_000 * 4],
        );
        scheduler.submit(handle.job.clone());
        let results = tokio::time::timeout(Duration::from_secs(5), handle.outcome)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].segments[0].text, "64000");
        assert_eq!(results[1].segments[0].text, "96000");
        assert_eq!(
            results[1].segments[0].start_ms, 4000,
            "second part is shifted by its offset"
        );

        let mut assigned = 0;
        while let Ok(event) = handle.events.try_recv() {
            if let JobEvent::Assigned { peer, .. } = event {
                assert_ne!(peer, "peer-50");
                assigned += 1;
            }
        }
        assert_eq!(assigned, 2);
    }

    #[tokio::test]
    async fn busy_peers_are_asked_again() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let scheduler = Scheduler::new(test_config(), db);
        tokio::spawn(scheduler.clone().run());
        // The only peer declines with "busy" first and accepts afterwards.
        let keys = PeerKeypair::generate();
        let (tx, mut rx) = mpsc::channel(64);
        let conn = Arc::new(PeerConn::new(Uuid::new_v4(), "moody".into(), keys.public_bytes(), tx));
        scheduler.add_peer(
            conn.clone(),
            device(),
            vec!["tiny".into()],
            false,
            Some(1.0),
            Some("tiny".into()),
        );
        let peer = conn.clone();
        tokio::spawn(async move {
            let mut offers = 0;
            while let Some(msg) = rx.recv().await {
                if let Outbound::Json(ServerMessage::Offer { job_id, .. }) = msg {
                    offers += 1;
                    if offers == 1 {
                        peer.resolve_offer(
                            job_id,
                            OfferAnswer {
                                accept: false,
                                reason: Some(DECLINE_BUSY.into()),
                            },
                        );
                    } else {
                        peer.resolve_offer(
                            job_id,
                            OfferAnswer {
                                accept: true,
                                reason: None,
                            },
                        );
                        let failure = PartEvent::Failed {
                            error: "done testing".into(),
                            retryable: false,
                        };
                        // Give the scheduler a moment to register the job, then end it.
                        let peer = peer.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            if let Some(job) = peer.active.lock().unwrap().values().next() {
                                let _ = job.events.send(failure);
                            }
                        });
                    }
                }
            }
        });
        let handle = Job::create(
            verbarelay_protocol::models::find("tiny"),
            None,
            false,
            vec![0; 16_000],
            &[],
        );
        scheduler.submit(handle.job.clone());
        let outcome = tokio::time::timeout(Duration::from_secs(10), handle.outcome)
            .await
            .unwrap()
            .unwrap();
        // Reached the second offer: the job ran (and was failed on purpose).
        assert!(outcome.unwrap_err().contains("done testing"));
    }

    #[tokio::test]
    async fn queue_timeout_fails_the_job() {
        let mut config = (*test_config()).clone();
        config.scheduling.queue_timeout_secs = 0;
        let scheduler = Scheduler::new(Arc::new(config), Arc::new(Db::open_in_memory().unwrap()));
        tokio::spawn(scheduler.clone().run());
        let handle = Job::create(None, None, false, vec![0; 16_000], &[]);
        scheduler.submit(handle.job.clone());
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle.outcome)
            .await
            .unwrap()
            .unwrap();
        assert!(outcome.unwrap_err().contains("no peer"));
    }
}
