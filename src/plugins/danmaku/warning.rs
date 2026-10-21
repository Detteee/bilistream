//! One process-local warning hold. Source ownership and generations keep late
//! room lookups, probes and old ffmpeg output from releasing a newer warning.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::logging::safe_text;
use crate::plugins::mask_pairing::PairRole;
use crate::plugins::warning_exempt::ReasonId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BroadcastSource {
    pub platform: String,
    pub channel_name: String,
    pub channel_id: String,
    pub episode_id: Option<String>,
}

impl BroadcastSource {
    pub fn new(platform: &str, name: &str, channel_id: &str, episode_id: Option<&str>) -> Self {
        Self {
            platform: platform.to_owned(),
            channel_name: name.to_owned(),
            channel_id: canonical_channel_id(platform, channel_id),
            episode_id: episode_id
                .filter(|id| !id.trim().is_empty())
                .map(str::to_owned),
        }
    }

    fn same_channel(&self, other: &Self) -> bool {
        self.platform == other.platform
            && !self.channel_id.is_empty()
            && self.channel_id == other.channel_id
    }
}

pub(crate) fn canonical_channel_id(platform: &str, channel_id: &str) -> String {
    if platform == "TW" {
        channel_id.trim().to_ascii_lowercase()
    } else {
        channel_id.trim().to_owned()
    }
}

#[derive(Clone, Debug)]
struct OwnedSource {
    session: u64,
    source: BroadcastSource,
}

#[derive(Clone, Debug)]
struct WarningHold {
    channel_name: String,
    source: Option<OwnedSource>,
    skip_logged: bool,
    /// Every class the held 违规原因 matched; empty means unknown, which never
    /// releases an exemption.
    reason_ids: BTreeSet<ReasonId>,
}

impl WarningHold {
    fn matches(&self, source: &BroadcastSource) -> bool {
        self.channel_name == source.channel_name
            || self
                .source
                .as_ref()
                .is_some_and(|held| held.source.same_channel(source))
    }
}

#[derive(Default)]
struct WarningState {
    generation: u64,
    event: u64,
    pending_event: Option<u64>,
    owner_epoch: u64,
    active: Option<OwnedSource>,
    hold: Option<WarningHold>,
}

#[derive(Default)]
pub(crate) struct WarningTracker(Mutex<WarningState>);

pub(crate) fn warning_tracker() -> Arc<WarningTracker> {
    static WARNINGS: OnceLock<Arc<WarningTracker>> = OnceLock::new();
    WARNINGS
        .get_or_init(|| Arc::new(WarningTracker::default()))
        .clone()
}

#[derive(Clone, Debug)]
pub(crate) struct WarningSnapshot {
    generation: u64,
    event: u64,
    owner_epoch: u64,
    hold: Option<WarningHold>,
}

impl WarningSnapshot {
    #[cfg(test)]
    pub fn has_hold(&self) -> bool {
        self.hold.is_some()
    }

    pub fn holds(&self, source: &BroadcastSource) -> bool {
        self.hold.as_ref().is_some_and(|hold| hold.matches(source))
    }

    pub fn holds_name(&self, name: &str) -> bool {
        self.hold
            .as_ref()
            .is_some_and(|hold| hold.channel_name == name)
    }

    /// Card identity: same platform/channel id. Display-name-only matching
    /// would light the YouTube card for a Twitch hold of the same talent.
    pub fn holds_identity(&self, source: &BroadcastSource) -> bool {
        let Some(hold) = self.hold.as_ref() else {
            return false;
        };
        if let Some(held) = hold.source.as_ref() {
            held.source.same_channel(source)
        } else {
            hold.channel_name == source.channel_name
        }
    }

    /// The matched reason classes of the current hold (empty = unknown).
    pub fn hold_reasons(&self) -> BTreeSet<ReasonId> {
        self.hold
            .as_ref()
            .map(|hold| hold.reason_ids.clone())
            .unwrap_or_default()
    }

    pub fn episode_id_for(&self, source: &BroadcastSource) -> Option<&str> {
        let held = self.hold.as_ref()?.source.as_ref()?;
        held.source
            .same_channel(source)
            .then_some(held.source.episode_id.as_deref())
            .flatten()
    }
}

/// Captured synchronously at packet receipt, before another command or room
/// HTTP request can change the running source. A title is only a fallback.
#[derive(Clone)]
pub(crate) struct WarningEvent {
    generation: u64,
    event: u64,
    owner_epoch: u64,
    source: Option<OwnedSource>,
    marked: bool,
    /// Matched reason classes, set at packet capture before the frame can
    /// change the running source.
    reason_ids: BTreeSet<ReasonId>,
    /// Mask/dialog pairing role, set at packet capture (None elsewhere).
    pair: Option<PairRole>,
}

impl WarningEvent {
    pub fn has_source(&self) -> bool {
        self.source.is_some()
    }

    /// Attach the classes classified from this frame's 违规原因 (empty for a
    /// missing reason or the mask).
    pub fn set_reason_ids(&mut self, reason_ids: BTreeSet<ReasonId>) {
        self.reason_ids = reason_ids;
    }

    /// The captured relayed source (name + identity) this frame arrived on.
    pub fn source(&self) -> Option<&BroadcastSource> {
        self.source.as_ref().map(|owned| &owned.source)
    }

    /// The classes classified from this frame's 违规原因.
    pub fn reason_ids(&self) -> &BTreeSet<ReasonId> {
        &self.reason_ids
    }

    /// The mask/dialog pairing role captured for this frame.
    pub fn pair(&self) -> Option<&PairRole> {
        self.pair.as_ref()
    }

    pub fn set_pair(&mut self, pair: Option<PairRole>) {
        self.pair = pair;
    }
}

pub(crate) struct SourceOwner {
    tracker: Arc<WarningTracker>,
    session: u64,
    owner_epoch: u64,
}

impl Drop for SourceOwner {
    fn drop(&mut self) {
        let mut state = self.tracker.lock();
        if state.owner_epoch == self.owner_epoch
            && state
                .active
                .as_ref()
                .is_some_and(|active| active.session == self.session)
        {
            state.active = None;
        }
    }
}

/// A hold released by the warning/cut exemption; the caller logs the R8 line
/// and records the R4 cooldown with the matched classes.
pub(crate) struct ExemptRelease {
    pub channel: String,
    pub reason_ids: BTreeSet<ReasonId>,
}

#[derive(Clone)]
pub(crate) struct PushWarningRelease {
    generation: u64,
    held_channel: String,
    source: BroadcastSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WarningReleaseReason {
    OtherChannelPushing,
    NewEpisode,
    Offline,
    ManualRestart,
    AdminCard,
    /// The configured exemption allowed this source to restart (R2/R4).
    Exempt,
}

impl WarningReleaseReason {
    fn label(self) -> &'static str {
        match self {
            Self::OtherChannelPushing => "另一频道已确认推流",
            Self::NewEpisode => "同频道已确认新的直播 ID",
            Self::Offline => "源频道已确认下播",
            Self::ManualRestart => "手动重启流",
            Self::AdminCard => "管理页卡片手动解除",
            Self::Exempt => "已豁免警告/切断暂停",
        }
    }
}

pub(crate) struct WarningRelease {
    channel: String,
    reason: WarningReleaseReason,
}

impl WarningRelease {
    pub fn log(self) {
        tracing::info!(
            "频道 {} 的临时跳过已解除；原因={}",
            safe_text(&self.channel, 64),
            self.reason.label()
        );
        crate::webui::state::request_status_refresh();
    }
}

/// A successful provider response, before warning/title/playability filters.
/// Failed requests are deliberately absent, never synthesized as offline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourcePresence {
    Live,
    Offline,
    Inconclusive,
}

pub(crate) struct SourceObservation {
    pub source: BroadcastSource,
    pub presence: SourcePresence,
}

impl SourceObservation {
    pub fn new(source: BroadcastSource, is_live: bool, scheduled_or_upcoming: bool) -> Self {
        Self {
            source,
            presence: if is_live {
                SourcePresence::Live
            } else if scheduled_or_upcoming {
                SourcePresence::Inconclusive
            } else {
                SourcePresence::Offline
            },
        }
    }
}

impl WarningTracker {
    fn lock(&self) -> MutexGuard<'_, WarningState> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Refine the classes of the current hold without needing an active
    /// source: a paired dialog can arrive after the relay owner was released.
    /// Never bumps the event generation, so queued frames stay current.
    pub fn refine_hold_reasons(&self, reason_ids: BTreeSet<ReasonId>) -> bool {
        let mut state = self.lock();
        let Some(hold) = state.hold.as_mut() else {
            return false;
        };
        if hold.reason_ids == reason_ids {
            return false;
        }
        hold.reason_ids = reason_ids;
        true
    }

    pub fn snapshot(&self) -> WarningSnapshot {
        let state = self.lock();
        WarningSnapshot {
            generation: state.generation,
            event: state.event,
            owner_epoch: state.owner_epoch,
            hold: state.hold.clone(),
        }
    }

    pub fn active_source(&self) -> Option<BroadcastSource> {
        self.lock()
            .active
            .as_ref()
            .map(|active| active.source.clone())
    }

    /// Called under the supervisor lock only after the process is registered.
    pub fn own_source(self: &Arc<Self>, session: u64, source: BroadcastSource) -> SourceOwner {
        let mut state = self.lock();
        state.owner_epoch = state.owner_epoch.wrapping_add(1);
        if state.pending_event.take().is_some() {
            state.event = state.event.wrapping_add(1);
        }
        state.active = Some(OwnedSource { session, source });
        SourceOwner {
            tracker: self.clone(),
            session,
            owner_epoch: state.owner_epoch,
        }
    }

    pub fn capture_event(&self) -> WarningEvent {
        let mut state = self.lock();
        state.event = state.event.wrapping_add(1);
        state.pending_event = Some(state.event);
        WarningEvent {
            generation: state.generation,
            event: state.event,
            owner_epoch: state.owner_epoch,
            source: state.active.clone(),
            marked: false,
            reason_ids: BTreeSet::new(),
            pair: None,
        }
    }

    /// A room_init poll is not itself a warning event. It must not supersede
    /// a WARNING/CUT_OFF whose room lookup is still in flight.
    pub fn snapshot_event(&self) -> WarningEvent {
        let state = self.lock();
        WarningEvent {
            generation: state.generation,
            event: state.event,
            owner_epoch: state.owner_epoch,
            source: state.active.clone(),
            marked: false,
            reason_ids: BTreeSet::new(),
            pair: None,
        }
    }

    fn event_matches(state: &WarningState, event: &WarningEvent) -> bool {
        state.generation == event.generation
            && state.event == event.event
            && state.owner_epoch == event.owner_epoch
    }

    pub fn event_is_current(&self, event: &WarningEvent) -> bool {
        let state = self.lock();
        if event.marked {
            // A later unconfirmed ROOM_LOCK cannot suppress an already
            // confirmed WARNING's stop. A newer hold/release/owner still can.
            state.generation == event.generation && state.owner_epoch == event.owner_epoch
        } else {
            Self::event_matches(&state, event)
        }
    }

    /// A failed/untimed room lookup is not a hold. Unblock later push samples
    /// while making that completed event unusable by delayed work.
    pub fn finish_unmarked_event(&self, event: &WarningEvent) {
        let mut state = self.lock();
        if Self::event_matches(&state, event) && state.pending_event == Some(event.event) {
            state.pending_event = None;
            state.event = state.event.wrapping_add(1);
        }
    }

    /// A confirmed timed lock uses this same path as WARNING/CUT_OFF.
    /// Returns the raw name to log after releasing the mutex.
    pub fn mark_event(
        &self,
        event: &mut WarningEvent,
        fallback_title: Option<&str>,
    ) -> Option<String> {
        let name = event
            .source
            .as_ref()
            .map(|owned| owned.source.channel_name.as_str())
            .filter(|name| !name.is_empty())
            .or_else(|| fallback_title.and_then(channel_from_bili_title))?;
        let mut state = self.lock();
        if !Self::event_matches(&state, event) {
            return None;
        }
        state.generation = state.generation.wrapping_add(1);
        state.pending_event = None;
        event.generation = state.generation;
        event.marked = true;
        state.hold = Some(WarningHold {
            channel_name: name.to_owned(),
            source: event.source.clone(),
            skip_logged: false,
            reason_ids: event.reason_ids.clone(),
        });
        Some(name.to_owned())
    }

    pub fn mark_timed_event(
        &self,
        event: &mut WarningEvent,
        fallback_title: Option<&str>,
    ) -> Option<String> {
        let existing = self.snapshot();
        if event
            .source
            .as_ref()
            .is_some_and(|source| existing.holds(&source.source))
            || (event.source.is_none()
                && fallback_title
                    .and_then(channel_from_bili_title)
                    .is_some_and(|name| existing.holds_name(name)))
        {
            return None;
        }
        self.mark_event(event, fallback_title)
    }

    pub fn take_skip_log(&self, source: &BroadcastSource) -> bool {
        let mut state = self.lock();
        let Some(hold) = state.hold.as_mut().filter(|hold| hold.matches(source)) else {
            return false;
        };
        !std::mem::replace(&mut hold.skip_logged, true)
    }

    pub fn push_release_token(&self, source: &BroadcastSource) -> Option<PushWarningRelease> {
        let state = self.lock();
        let hold = state.hold.as_ref()?;
        (!hold.matches(source)).then(|| PushWarningRelease {
            generation: state.generation,
            held_channel: hold.channel_name.clone(),
            source: source.clone(),
        })
    }

    /// The caller still owns the Push reader under the supervisor lock. This
    /// second guard prevents output from clearing a warning created mid-run.
    pub fn observe_push(&self, session: u64, token: &PushWarningRelease) -> Option<WarningRelease> {
        let mut state = self.lock();
        if state.generation != token.generation
            || state.pending_event.is_some()
            || !state
                .active
                .as_ref()
                .is_some_and(|active| active.session == session && active.source == token.source)
            || !state.hold.as_ref().is_some_and(|hold| {
                hold.channel_name == token.held_channel && !hold.matches(&token.source)
            })
        {
            return None;
        }
        Self::release(&mut state, WarningReleaseReason::OtherChannelPushing)
    }

    pub fn observe_sources(
        &self,
        before_probe: &WarningSnapshot,
        observations: &[SourceObservation],
    ) -> Option<WarningRelease> {
        let mut state = self.lock();
        if state.generation != before_probe.generation
            || state.event != before_probe.event
            || state.owner_epoch != before_probe.owner_epoch
            || state.pending_event.is_some()
        {
            return None;
        }
        let hold = state.hold.as_ref()?;
        let new_episode = hold.source.as_ref().is_some_and(|held| {
            matches!(held.source.platform.as_str(), "YT" | "TW")
                && observations.iter().any(|observed| {
                    observed.presence == SourcePresence::Live
                        && held.source.same_channel(&observed.source)
                        && matches!((&held.source.episode_id, &observed.source.episode_id),
                            (Some(old), Some(new)) if old != new)
                })
        });
        let offline = observations.iter().any(|observed| {
            hold.matches(&observed.source) && observed.presence == SourcePresence::Offline
        }) && !observations.iter().any(|observed| {
            hold.matches(&observed.source) && observed.presence != SourcePresence::Offline
        });
        if new_episode {
            Self::release(&mut state, WarningReleaseReason::NewEpisode)
        } else if offline {
            Self::release(&mut state, WarningReleaseReason::Offline)
        } else {
            None
        }
    }

    pub fn release_name(
        &self,
        before_action: &WarningSnapshot,
        name: &str,
        reason: WarningReleaseReason,
    ) -> Option<WarningRelease> {
        let mut state = self.lock();
        if state.generation != before_action.generation
            || state.event != before_action.event
            || state
                .hold
                .as_ref()
                .is_none_or(|hold| hold.channel_name != name)
        {
            return None;
        }
        Self::release(&mut state, reason)
    }

    pub fn release_source(
        &self,
        before_action: &WarningSnapshot,
        source: &BroadcastSource,
        reason: WarningReleaseReason,
    ) -> Option<WarningRelease> {
        let mut state = self.lock();
        if state.generation != before_action.generation
            || state.event != before_action.event
            || !state.hold.as_ref().is_some_and(|hold| hold.matches(source))
        {
            return None;
        }
        Self::release(&mut state, reason)
    }

    pub fn clear(&self, reason: WarningReleaseReason) -> Option<WarningRelease> {
        Self::release(&mut self.lock(), reason)
    }

    /// Release the hold matching `source` because the exemption allows the
    /// restart. `before_action` must be the snapshot the exemption decision was
    /// taken on: a warning marked in between (the cooldown write is I/O) may carry
    /// different classes or episode, so a stale release is refused. Returns the
    /// channel and matched classes for the R8 log and the R4 cooldown; every later
    /// `holds()` check then sees no hold.
    pub fn release_exempt(
        &self,
        before_action: &WarningSnapshot,
        source: &BroadcastSource,
    ) -> Option<ExemptRelease> {
        let mut state = self.lock();
        if state.generation != before_action.generation || state.event != before_action.event {
            return None;
        }
        let hold = state.hold.as_ref().filter(|hold| hold.matches(source))?;
        let released = ExemptRelease {
            channel: hold.channel_name.clone(),
            reason_ids: hold.reason_ids.clone(),
        };
        Self::release(&mut state, WarningReleaseReason::Exempt)?;
        Some(released)
    }

    fn release(state: &mut WarningState, reason: WarningReleaseReason) -> Option<WarningRelease> {
        state.generation = state.generation.wrapping_add(1);
        state.event = state.event.wrapping_add(1);
        state.pending_event = None;
        state.hold.take().map(|hold| WarningRelease {
            channel: hold.channel_name,
            reason,
        })
    }
}

pub(crate) fn channel_from_bili_title(title: &str) -> Option<&str> {
    title
        .rsplit_once("【转播】")
        .map(|(_, name)| name.trim())
        .filter(|name| !name.is_empty())
}

pub(crate) fn log_warning_mark(name: &str) {
    tracing::info!(
        "🚫 已标记频道 {} 为警告状态，将跳过转播",
        safe_text(name, 64)
    );
    crate::webui::state::request_status_refresh();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(platform: &str, channel: &str, episode: Option<&str>) -> BroadcastSource {
        BroadcastSource::new(platform, channel, &format!("{channel}-id"), episode)
    }

    fn mark(tracker: &Arc<WarningTracker>, source: BroadcastSource, session: u64) -> SourceOwner {
        let owner = tracker.own_source(session, source.clone());
        let mut event = tracker.capture_event();
        assert_eq!(
            tracker.mark_event(&mut event, None).as_deref(),
            Some(source.channel_name.as_str())
        );
        owner
    }

    #[test]
    fn only_a_new_nonempty_live_id_in_the_same_namespace_releases() {
        for platform in ["YT", "TW"] {
            let tracker = Arc::new(WarningTracker::default());
            let held = source(platform, "示例频道 001", Some("episode-1"));
            let _owner = mark(&tracker, held.clone(), 1);
            let before = tracker.snapshot();
            for candidate in [
                held.clone(),
                source(platform, "示例频道 001", None),
                source(platform, "示例频道 001", Some("")),
                source(platform, "示例频道 001", Some("   ")),
                source(platform, "示例频道 002", Some("episode-2")),
                source(
                    if platform == "YT" { "TW" } else { "YT" },
                    "示例频道 001",
                    Some("episode-2"),
                ),
            ] {
                assert!(tracker
                    .observe_sources(&before, &[SourceObservation::new(candidate, true, false)])
                    .is_none());
                assert!(tracker.snapshot().holds(&held));
            }
            // No result is an error/missing observation, never an offline fact.
            assert!(tracker.observe_sources(&before, &[]).is_none());
            let scheduled = SourceObservation::new(
                source(platform, "示例频道 001", Some("episode-2")),
                false,
                true,
            );
            assert!(tracker.observe_sources(&before, &[scheduled]).is_none());
            let new_live = SourceObservation::new(
                source(platform, "示例频道 001", Some("episode-2")),
                true,
                false,
            );
            assert_eq!(
                tracker
                    .observe_sources(&before, &[new_live])
                    .unwrap()
                    .reason,
                WarningReleaseReason::NewEpisode
            );
            assert!(!tracker.snapshot().holds(&held));
        }
    }

    #[test]
    fn live_or_upcoming_same_broadcaster_defeats_another_offline_answer() {
        let tracker = Arc::new(WarningTracker::default());
        let held = source("YT", "示例频道 001", Some("episode-1"));
        let _owner = mark(&tracker, held.clone(), 1);
        let before = tracker.snapshot();
        for presence in [SourcePresence::Live, SourcePresence::Inconclusive] {
            let observations = [
                SourceObservation::new(held.clone(), false, false),
                SourceObservation {
                    source: source("TW", "示例频道 001", Some("unrelated-platform-id")),
                    presence,
                },
            ];
            assert!(tracker.observe_sources(&before, &observations).is_none());
        }
        assert_eq!(
            tracker
                .observe_sources(
                    &before,
                    &[SourceObservation::new(held.clone(), false, false)]
                )
                .unwrap()
                .reason,
            WarningReleaseReason::Offline
        );
        assert!(!tracker.snapshot().holds(&held));
    }

    #[test]
    fn generation_and_current_owner_reject_late_release_evidence() {
        let tracker = Arc::new(WarningTracker::default());
        let held = source("YT", "示例频道 001", Some("episode-1"));
        let _owner = mark(&tracker, held.clone(), 1);
        let before_old_probe = tracker.snapshot();
        let other = source("TW", "示例频道 002", Some("episode-b"));
        let token = tracker.push_release_token(&other).unwrap();
        let _other_owner = tracker.own_source(2, other.clone());
        assert!(tracker
            .observe_sources(
                &before_old_probe,
                &[SourceObservation::new(
                    source("YT", "示例频道 001", Some("episode-2")),
                    true,
                    false
                )]
            )
            .is_none());
        let before_new_warning = tracker.snapshot();
        let mut next_warning = tracker.capture_event();
        tracker.mark_event(&mut next_warning, None).unwrap();
        assert!(tracker.observe_push(2, &token).is_none());
        assert!(tracker
            .release_name(
                &before_new_warning,
                &held.channel_name,
                WarningReleaseReason::AdminCard
            )
            .is_none());
        assert!(tracker.snapshot().holds(&other));
        let stale_token = tracker.push_release_token(&held).unwrap();
        let _replacement = tracker.own_source(3, held);
        assert!(tracker.observe_push(2, &stale_token).is_none());
        assert!(tracker.snapshot().holds(&other));
    }

    #[tokio::test]
    async fn a_pending_timed_lock_blocks_older_push_probe_and_manual_release() {
        let tracker = Arc::new(WarningTracker::default());
        let a = source("YT", "示例频道 001", Some("episode-1"));
        let _a_owner = mark(&tracker, a.clone(), 1);
        let b = source("TW", "示例频道 002", Some("episode-b"));
        let push = tracker.push_release_token(&b).unwrap();
        let _b_owner = tracker.own_source(2, b.clone());
        let before = tracker.snapshot();
        let event = tracker.capture_event();
        let (confirmed, delayed_room_init) = tokio::sync::oneshot::channel();
        let work = tokio::spawn({
            let tracker = tracker.clone();
            async move {
                let mut event = event;
                delayed_room_init.await.unwrap();
                tracker.mark_timed_event(&mut event, None)
            }
        });
        assert!(tracker.observe_push(2, &push).is_none());
        assert!(tracker
            .observe_sources(
                &before,
                &[SourceObservation::new(
                    source("YT", "示例频道 001", Some("episode-2")),
                    true,
                    false
                )]
            )
            .is_none());
        assert!(tracker
            .release_name(&before, &a.channel_name, WarningReleaseReason::AdminCard)
            .is_none());
        assert!(tracker
            .release_name(
                &before,
                &a.channel_name,
                WarningReleaseReason::ManualRestart
            )
            .is_none());
        confirmed.send(()).unwrap();
        assert_eq!(
            work.await.unwrap().as_deref(),
            Some(b.channel_name.as_str())
        );
        assert!(tracker.snapshot().holds(&b));
        assert!(tracker.observe_push(2, &push).is_none());
    }

    #[test]
    fn failed_or_untimed_room_lookup_keeps_the_old_hold_and_allows_later_push() {
        let tracker = Arc::new(WarningTracker::default());
        let a = source("YT", "示例频道 001", Some("episode-1"));
        let _a_owner = mark(&tracker, a.clone(), 1);
        let b = source("TW", "示例频道 002", Some("episode-b"));
        let push = tracker.push_release_token(&b).unwrap();
        let _b_owner = tracker.own_source(2, b.clone());
        let failed = tracker.capture_event();
        assert!(tracker.observe_push(2, &push).is_none());
        tracker.finish_unmarked_event(&failed);
        assert!(tracker.snapshot().holds(&a));
        assert!(!tracker.snapshot().holds(&b));
        assert!(tracker.observe_push(2, &push).is_some());
        let mut late = failed;
        assert!(tracker.mark_timed_event(&mut late, None).is_none());
    }

    #[test]
    fn an_unconfirmed_room_lock_does_not_suppress_a_confirmed_warning_stop() {
        let tracker = Arc::new(WarningTracker::default());
        let a = source("YT", "示例频道 001", Some("episode-1"));
        let _owner = tracker.own_source(1, a.clone());
        let mut confirmed_warning = tracker.capture_event();
        tracker.mark_event(&mut confirmed_warning, None).unwrap();
        let pending_room_lock = tracker.capture_event();
        assert!(tracker.event_is_current(&confirmed_warning));
        tracker.finish_unmarked_event(&pending_room_lock);
        assert!(tracker.event_is_current(&confirmed_warning));
        tracker.clear(WarningReleaseReason::ManualRestart);
        assert!(!tracker.event_is_current(&confirmed_warning));
    }

    #[test]
    fn title_fallback_and_captured_source_cannot_attach_to_the_next_session() {
        let tracker = Arc::new(WarningTracker::default());
        let mut no_owner = tracker.capture_event();
        assert!(tracker
            .mark_event(&mut no_owner, Some("普通直播标题"))
            .is_none());
        assert_eq!(
            tracker
                .mark_event(&mut no_owner, Some("【转播】示例频道 001"))
                .as_deref(),
            Some("示例频道 001")
        );
        let stale_fallback = tracker.capture_event();
        let original = source("YT", "示例频道 002", Some("episode-2"));
        let _owner = tracker.own_source(2, original.clone());
        let mut old = stale_fallback;
        assert!(tracker
            .mark_event(&mut old, Some("【转播】示例频道 002"))
            .is_none());
        let mut captured = tracker.capture_event();
        tracker
            .mark_event(&mut captured, Some("【转播】错误的后到标题"))
            .unwrap();
        assert!(tracker.snapshot().holds(&original));
        let replacement = source("TW", "示例频道 003", Some("episode-3"));
        let _replacement = tracker.own_source(3, replacement.clone());
        assert!(tracker
            .mark_event(&mut captured, Some("【转播】示例频道 003"))
            .is_none());
        assert!(tracker.snapshot().holds(&original));
        assert!(!tracker.snapshot().holds(&replacement));
    }

    #[test]
    fn identity_hold_does_not_light_the_other_platform_of_the_same_display_name() {
        let tracker = Arc::new(WarningTracker::default());
        let held = source("TW", "示例频道 001", Some("stream-1"));
        let _owner = mark(&tracker, held.clone(), 1);
        let snap = tracker.snapshot();
        assert!(snap.holds_identity(&held));
        assert!(snap.holds(&source("YT", "示例频道 001", Some("other"))));
        assert!(!snap.holds_identity(&source("YT", "示例频道 001", Some("other"))));
    }

    #[test]
    fn raw_long_names_and_provider_channel_identity_are_not_log_labels() {
        let tracker = Arc::new(WarningTracker::default());
        let raw = "示例频道📺".repeat(20);
        let held = BroadcastSource::new("TW", &raw, "SomeLogin", Some("stream-1"));
        let _owner = mark(&tracker, held.clone(), 1);
        assert!(tracker.snapshot().holds_name(&raw));
        assert!(tracker.push_release_token(&held).is_none());
        let different_alias =
            BroadcastSource::new("TW", "另一个显示名", "somelogin", Some("stream-1"));
        assert!(tracker.snapshot().holds(&different_alias));
        let changed = BroadcastSource::new("TW", &raw, "SOMELOGIN", Some("stream-2"));
        assert!(tracker
            .observe_sources(
                &tracker.snapshot(),
                &[SourceObservation::new(changed, true, false)]
            )
            .is_some());
        let fresh_process = WarningTracker::default();
        assert!(!fresh_process.snapshot().has_hold());
        assert_ne!(
            BroadcastSource::new("YT", "a", "UCAb", None).channel_id,
            BroadcastSource::new("YT", "a", "UCab", None).channel_id
        );
    }

    #[test]
    fn a_stale_exempt_release_never_clears_a_newer_hold() {
        let tracker = Arc::new(WarningTracker::default());
        let source = BroadcastSource::new("YT", "n", "UC1", Some("ep-1"));
        let _owner = tracker.own_source(1, source.clone());
        let mut first = tracker.capture_event();
        first.set_reason_ids([ReasonId::IdleRecording].into_iter().collect());
        tracker.mark_event(&mut first, None).unwrap();
        let decided_on = tracker.snapshot();

        // Another frame replaces the hold after the decision snapshot.
        let mut second = tracker.capture_event();
        second.set_reason_ids([ReasonId::GameInfoViolation].into_iter().collect());
        tracker.mark_event(&mut second, None).unwrap();
        assert!(tracker.release_exempt(&decided_on, &source).is_none());
        assert!(
            tracker.snapshot().holds(&source),
            "the newer hold must survive"
        );
        assert_eq!(
            tracker.snapshot().hold_reasons(),
            [ReasonId::GameInfoViolation].into_iter().collect()
        );
    }

    #[test]
    fn a_marked_hold_keeps_the_captured_reason_classes() {
        let tracker = Arc::new(WarningTracker::default());
        let source = BroadcastSource::new("YT", "n", "UC1", Some("ep-1"));
        let _owner = mark(&tracker, source.clone(), 1);
        let mut event = tracker.capture_event();
        event.set_reason_ids([ReasonId::IdleRecording].into_iter().collect());
        tracker.mark_event(&mut event, None).unwrap();
        assert_eq!(
            tracker.snapshot().hold_reasons(),
            [ReasonId::IdleRecording].into_iter().collect()
        );
    }

    #[test]
    fn an_exempt_release_clears_the_hold_and_reports_the_classes() {
        let tracker = Arc::new(WarningTracker::default());
        let source = BroadcastSource::new("YT", "n", "UC1", Some("ep-1"));
        let _owner = tracker.own_source(1, source.clone());
        let mut event = tracker.capture_event();
        event.set_reason_ids([ReasonId::IdleRecording].into_iter().collect());
        tracker.mark_event(&mut event, None).unwrap();
        let other = BroadcastSource::new("TW", "o", "tw1", None);
        let before = tracker.snapshot();
        assert!(
            tracker.release_exempt(&before, &other).is_none(),
            "different source"
        );
        let released = tracker.release_exempt(&before, &source).unwrap();
        assert_eq!(released.channel, "n");
        assert_eq!(
            released.reason_ids,
            [ReasonId::IdleRecording].into_iter().collect()
        );
        assert!(!tracker.snapshot().holds(&source), "the hold is gone");
        let stale = tracker.snapshot();
        assert!(
            tracker.release_exempt(&stale, &source).is_none(),
            "only once"
        );
    }
}
