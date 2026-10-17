//! Map shared runtime facts to the already deployed broadcast-notice state
//! machine. Cluster alerts and configured watch episodes retain their sole
//! level-triggered observer; history-only facts never create new QQ alerts.

use super::live_events::{LiveSignal, LiveSource};
use crate::events::{Event, EventKind};

pub fn wants(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::BroadcastStarted { .. } | EventKind::BroadcastEnded { .. }
    )
}

pub fn map(event: &Event) -> Option<LiveSignal> {
    match &event.kind {
        EventKind::BroadcastStarted {
            stream_key,
            platform,
            channel,
            title,
        } => Some(LiveSignal::Source(LiveSource {
            stream_key: stream_key.clone(),
            platform: platform.clone(),
            channel: channel.clone(),
            title: title.clone(),
        })),
        EventKind::BroadcastEnded { cause } => Some(LiveSignal::Ended(*cause)),
        _ => None,
    }
}
