use std::convert::Infallible;

use super::listen::{AuthGeneration, AuthState};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::{extract::State, http::HeaderMap, Extension};
use futures_util::stream::Stream;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::AppState;

/// Dashboard status snapshot changed (stream live state, titles, toggles).
pub const STATUS: &str = "status";
/// Cluster topology or node state changed (owner, role, health, sync state).
pub const CLUSTER: &str = "cluster";
/// Persisted configuration changed.
pub const CONFIG: &str = "config";
/// A Holodex panel list (channels or favorites) changed.
pub const HOLODEX: &str = "holodex";

/// Notify all connected WebUI clients that `kind` changed. Never blocks; if no
/// client is connected the event is dropped.
pub fn publish(kind: &'static str) {
    AppState::current().publish(kind);
}

/// SSE endpoint: emits a named event whenever server-side state changes so the
/// WebUI can refetch immediately instead of waiting for its poll interval.
pub(super) async fn sse_events(
    State(state): State<AppState>,
    Extension(auth): Extension<Arc<AuthState>>,
    Extension(generation): Extension<AuthGeneration>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe before inspecting authority: no change between middleware and
    // stream construction, or between idle events, can strand an authenticated stream.
    let changes = auth.changed.subscribe();
    let rx = state.subscribe_events();
    let stream = futures_util::stream::unfold(
        (rx, changes, auth, headers, generation),
        |(mut rx, mut changes, auth, headers, generation)| async move {
            if !auth
                .access(&headers)
                .is_ok_and(|(snapshot, valid)| valid && snapshot.revision == generation.0)
            {
                return None;
            }
            let event = tokio::select! {
                _ = changes.changed() => return None,
                event = rx.recv() => event,
            };
            if !auth
                .access(&headers)
                .is_ok_and(|(snapshot, valid)| valid && snapshot.revision == generation.0)
            {
                return None;
            }
            let event = match event {
                Ok(kind) => Event::default().event(kind).data("changed"),
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    Event::default().event("refresh").data("all")
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            };
            Some((Ok(event), (rx, changes, auth, headers, generation)))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webui::{listen::require_webui_auth, sessions::PasswordMutation};
    use axum::{
        body::Body,
        http::{header, Request},
        middleware,
        routing::get,
        Router,
    };
    use futures_util::StreamExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn credential_change_closes_idle_stream_and_subscribe_race_without_client_cooperation() {
        let root = std::env::temp_dir().join(format!("bilistream-auth-sse-{}", std::process::id()));
        let store =
            crate::storage::Store::open(root.join("data"), root.join("keys/master"), None).unwrap();
        let auth = AuthState::open(store.clone(), Some("synthetic".into())).unwrap();
        let revision = auth.sessions.snapshot().unwrap().revision;
        let token = auth
            .sessions
            .issue(revision, None, crate::storage::now())
            .unwrap();
        let state = AppState::new();
        let app = Router::new()
            .route("/api/events", get(sse_events))
            .layer(middleware::from_fn(require_webui_auth))
            .layer(Extension(auth.clone()))
            .with_state(state.clone());
        let request = || {
            Request::builder()
                .uri("/api/events")
                .header(header::COOKIE, format!("bilistream_session={token}"))
                .body(Body::empty())
                .unwrap()
        };
        let response = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        state.publish(STATUS);
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&first).contains("status"));
        // Construct a second response but do not poll its body before the change.
        let raced = app.clone().oneshot(request()).await.unwrap();
        auth.sessions
            .mutate(PasswordMutation::new(revision, Some("synthetic".into()), false).unwrap())
            .unwrap();
        auth.notify_changed();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
                .await
                .unwrap()
                .is_none()
        );
        let mut raced = raced.into_body().into_data_stream();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), raced.next())
                .await
                .unwrap()
                .is_none()
        );
        drop((stream, raced, app, auth, store));
        std::fs::remove_dir_all(root).unwrap();
    }
}
