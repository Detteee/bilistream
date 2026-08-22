use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use tokio::sync::broadcast;

use crate::AppState;

/// Dashboard status snapshot changed (stream live state, titles, toggles).
pub const STATUS: &str = "status";
/// Cluster topology or node state changed (owner, role, health, sync state).
pub const CLUSTER: &str = "cluster";
/// Persisted configuration changed.
pub const CONFIG: &str = "config";

/// Notify all connected WebUI clients that `kind` changed. Never blocks; if no
/// client is connected the event is dropped.
pub fn publish(kind: &'static str) {
    AppState::current().publish(kind);
}

/// SSE endpoint: emits a named event whenever server-side state changes so the
/// WebUI can refetch immediately instead of waiting for its poll interval.
pub async fn sse_events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.subscribe_events();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(kind) => Some((Ok(Event::default().event(kind).data("changed")), rx)),
            // Slow client missed events: tell it to refresh everything.
            Err(broadcast::error::RecvError::Lagged(_)) => {
                Some((Ok(Event::default().event("refresh").data("all")), rx))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}
