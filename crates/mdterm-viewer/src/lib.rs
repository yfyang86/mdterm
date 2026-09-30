//! mdterm-viewer: HTML viewer server (axum + SSE).
//!
//! Routes:
//!   GET `/`            → single-page HTML app (vendored assets, no CDN)
//!   GET `/events`      → SSE stream of full-session JSON on each update
//!   GET `/api/session` → current [`Session`] JSON
//!   GET `/assets/...`  → vendored JS/CSS/font assets (embedded at compile time)

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Json, Response,
    },
    routing::get,
    Router,
};
use mdterm_core::{CliKind, Session, TranscriptEvent};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

pub mod assets;

/// Shared application state.
struct AppState {
    /// The current session plus a monotonically increasing sequence number
    /// (0 = the initial empty session). The sequence lets SSE clients
    /// dedupe the subscribe-then-snapshot bootstrap (F8).
    session: RwLock<(u64, Session)>,
    updates: broadcast::Sender<(u64, Session)>,
}

/// The viewer server: shared state is an `Arc<RwLock<Session>>` (plus an
/// SSE fan-out channel). Construct via [`ViewerServer::start`].
pub struct ViewerServer {
    #[allow(dead_code)] // state lives in the spawned tasks; kept for introspection
    state: Arc<AppState>,
}

impl ViewerServer {
    /// Bind 127.0.0.1:<port> (port=0 → OS-assigned; the actual port is
    /// returned). Consumes the transcript event stream: each
    /// `TranscriptEvent::Updated` replaces the current session and is
    /// broadcast to all connected SSE clients.
    pub async fn start(
        port: u16,
        mut rx: mpsc::Receiver<TranscriptEvent>,
    ) -> anyhow::Result<u16> {
        let state = Arc::new(AppState {
            session: RwLock::new((0, Session::empty(CliKind::Claude))),
            updates: broadcast::channel(64).0,
        });

        // Pump transcript events into shared state + SSE fan-out.
        {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    match ev {
                        TranscriptEvent::Updated(session) => {
                            let seq = {
                                let mut guard = state.session.write().unwrap();
                                guard.0 += 1;
                                guard.1 = session.clone();
                                guard.0
                            };
                            let _ = state.updates.send((seq, session)); // ok if no listeners
                        }
                        TranscriptEvent::Error(e) => {
                            tracing::warn!("transcript watch error: {e}");
                        }
                    }
                }
            });
        }

        let app = Router::new()
            .route("/", get(index))
            .route("/events", get(events))
            .route("/api/session", get(api_session))
            .route("/assets/{*path}", get(asset))
            .with_state(Arc::clone(&state));

        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let actual_port = listener.local_addr()?.port();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("viewer server error: {e}");
            }
        });

        let _server = ViewerServer { state };
        Ok(actual_port)
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

async fn api_session(State(state): State<Arc<AppState>>) -> Json<Session> {
    Json(state.session.read().unwrap().1.clone())
}

async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    // F8: subscribe FIRST, then snapshot. With the old snapshot-then-
    // subscribe order, an update landing in between was lost (fatal for the
    // transcript's last update). With this order such an update is
    // delivered twice — once in the snapshot, once by the broadcast — so
    // the broadcast side is deduped by sequence number: anything at or
    // below the snapshot's sequence is skipped.
    let rx = state.updates.subscribe();
    let (cur_seq, current) = state.session.read().unwrap().clone();
    let last_seq = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(cur_seq));
    let stream = tokio_stream::once(current)
        .chain(
            BroadcastStream::new(rx)
                .filter_map(|r| r.ok())
                .filter_map(move |(seq, session)| {
                    let prev = last_seq.fetch_max(seq, std::sync::atomic::Ordering::SeqCst);
                    if seq > prev {
                        Some(session)
                    } else {
                        None // replay of the snapshot (or older): already sent
                    }
                }),
        )
        .map(|session| {
            let data = serde_json::to_string(&session)
                .unwrap_or_else(|_| "{\"id\":\"\",\"kind\":\"claude\",\"messages\":[]}".into());
            Ok(Event::default().data(data))
        });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn asset(Path(path): Path<String>) -> Response {
    match assets::lookup(&path) {
        Some((content_type, body)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            body,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
