//! Mask/dialog pairing for warning frames.
//!
//! `FULL_SCREEN_MASK_OPEN` never carries a reason; its companion is an
//! `ANCHOR_ECOLOGY_LIVING_DIALOG` on the same connection (one room per
//! connection), paired by arrival time because the frames share no id. The
//! mask's danmaku waits up to [`PAIR_WINDOW`] for that dialog; the dialog, once
//! paired, is only an update: the mask frame owns the single stop/notice.
//!
//! A dialog arriving before the mask is the leader (it already stopped and
//! noticed); the later mask then only adopts the dialog's reason classes for
//! the hold and sends nothing.
//!
//! The danmaku window is [`PAIR_WINDOW`]; the *hold* accepts the dialog for
//! the mask's `overlay_second` (default 60 s): inside that longer window the
//! dialog only refines the unmatched hold's reason classes (no second stop or
//! notice), so the exemption decision can still release it.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

use crate::plugins::warning_exempt::ReasonId;

/// The largest gap the two frames may have for the danmaku decision; either
/// order.
pub(crate) const PAIR_WINDOW: Duration = Duration::from_secs(10);

/// The mask's own overlay window when the frame carries no usable value.
pub(crate) const DEFAULT_OVERLAY: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub(crate) enum PairRole {
    /// First mask: stop immediately, then wait for the dialog (or the window)
    /// before choosing the notice.
    MaskLeader(Arc<Notify>),
    /// Mask paired with a dialog that already stopped and noticed; only adopt
    /// its reason classes, send nothing.
    MaskFollower { reason_ids: BTreeSet<ReasonId> },
    /// Dialog paired with the mask leader; the leader sends, so this frame
    /// must not stop or notice again.
    DialogFollower,
    /// Dialog inside the mask's `overlay_second` but after the danmaku window:
    /// only refine the hold's classes, never stop or notice again.
    HoldRefinement,
    /// Dialog with no mask before it; it acts like today.
    DialogLeader,
}

impl PairRole {
    pub(crate) fn is_follower(&self) -> bool {
        matches!(
            self,
            Self::MaskFollower { .. } | Self::DialogFollower | Self::HoldRefinement
        )
    }
}

struct PendingMask {
    at: Instant,
    notify: Arc<Notify>,
}

struct DialogMark {
    at: Instant,
    reason_ids: BTreeSet<ReasonId>,
}

struct MaskMark {
    at: Instant,
    overlay: Duration,
}

#[derive(Default)]
struct State {
    pending_mask: Option<PendingMask>,
    last_mask: Option<MaskMark>,
    last_dialog: Option<DialogMark>,
}

fn states() -> &'static Mutex<HashMap<u64, State>> {
    static STATES: OnceLock<Mutex<HashMap<u64, State>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_states() -> std::sync::MutexGuard<'static, HashMap<u64, State>> {
    states()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn prune(state: &mut State, now: Instant) {
    if state
        .pending_mask
        .as_ref()
        .is_some_and(|mask| now.saturating_duration_since(mask.at) > PAIR_WINDOW)
    {
        state.pending_mask = None;
    }
    if state
        .last_dialog
        .as_ref()
        .is_some_and(|dialog| now.saturating_duration_since(dialog.at) > PAIR_WINDOW)
    {
        state.last_dialog = None;
    }
    if state
        .last_mask
        .as_ref()
        .is_some_and(|mask| now.saturating_duration_since(mask.at) > mask.overlay)
    {
        state.last_mask = None;
    }
}

/// Register a mask frame. The leader gets the notify the stop task waits on;
/// a mask paired with a recent dialog is a follower and gets that dialog's
/// classes instead.
pub(crate) fn note_mask(room: u64, now: Instant, overlay: Duration) -> PairRole {
    let mut states = lock_states();
    let state = states.entry(room).or_default();
    prune(state, now);
    state.last_mask = Some(MaskMark { at: now, overlay });
    if let Some(dialog) = state.last_dialog.take() {
        state.pending_mask = None;
        return PairRole::MaskFollower {
            reason_ids: dialog.reason_ids,
        };
    }
    let notify = Arc::new(Notify::new());
    state.pending_mask = Some(PendingMask {
        at: now,
        notify: notify.clone(),
    });
    PairRole::MaskLeader(notify)
}

/// Register a dialog frame. A pending mask inside the window wakes up and owns
/// the stop/notice (dialog follower); otherwise the dialog acts (leader).
pub(crate) fn note_dialog(room: u64, now: Instant, reason_ids: BTreeSet<ReasonId>) -> PairRole {
    let mut states = lock_states();
    let state = states.entry(room).or_default();
    prune(state, now);
    state.last_dialog = Some(DialogMark {
        at: now,
        reason_ids,
    });
    if let Some(mask) = state.pending_mask.as_ref() {
        mask.notify.notify_one();
        return PairRole::DialogFollower;
    }
    if let Some(mask) = state.last_mask.take() {
        if now.saturating_duration_since(mask.at) <= mask.overlay {
            return PairRole::HoldRefinement;
        }
    }
    PairRole::DialogLeader
}

/// Drop the finished mask (the leader sends or times out).
pub(crate) fn finish_mask(room: u64) {
    if let Some(state) = lock_states().get_mut(&room) {
        state.pending_mask = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ids: &[ReasonId]) -> BTreeSet<ReasonId> {
        ids.iter().copied().collect()
    }

    fn assert_leader(role: &PairRole) {
        assert!(matches!(role, PairRole::MaskLeader(_)), "{role:?}");
    }

    #[test]
    fn a_dialog_after_the_mask_wakes_the_leader_and_stays_a_follower() {
        let room = 900_001;
        let start = Instant::now();
        assert_leader(&note_mask(room, start, DEFAULT_OVERLAY));
        assert!(matches!(
            note_dialog(
                room,
                start + Duration::from_secs(2),
                ids(&[ReasonId::IdleRecording]),
            ),
            PairRole::DialogFollower
        ));
        finish_mask(room);
    }

    #[test]
    fn a_dialog_before_the_mask_acts_and_the_mask_only_follows_its_classes() {
        let room = 900_002;
        let start = Instant::now();
        assert!(matches!(
            note_dialog(room, start, ids(&[ReasonId::ContentUnsuitable])),
            PairRole::DialogLeader
        ));
        match note_mask(room, start + Duration::from_secs(2), DEFAULT_OVERLAY) {
            PairRole::MaskFollower { reason_ids } => {
                assert_eq!(reason_ids, ids(&[ReasonId::ContentUnsuitable]));
            }
            other => panic!("expected follower, got {other:?}"),
        }
    }

    #[test]
    fn no_dialog_leaves_the_mask_a_leader_and_the_window_expires() {
        let room = 900_003;
        let start = Instant::now();
        // A short overlay so the longer hold window can be observed expiring.
        assert_leader(&note_mask(room, start, Duration::from_secs(5)));
        finish_mask(room);
        assert!(matches!(
            note_dialog(room, start + Duration::from_secs(4), ids(&[])),
            PairRole::HoldRefinement
        ));
        // A dialog after the overlay no longer pairs with the old mask.
        assert!(matches!(
            note_dialog(room, start + Duration::from_secs(6), ids(&[])),
            PairRole::DialogLeader
        ));
        assert_leader(&note_mask(
            room,
            start + PAIR_WINDOW * 2 + Duration::from_secs(2),
            DEFAULT_OVERLAY,
        ));
        finish_mask(room);
    }

    #[test]
    fn a_stale_dialog_does_not_capture_a_later_mask() {
        let room = 900_004;
        let start = Instant::now();
        assert!(matches!(
            note_dialog(room, start, ids(&[ReasonId::IdleRecording])),
            PairRole::DialogLeader
        ));
        assert_leader(&note_mask(
            room,
            start + PAIR_WINDOW + Duration::from_secs(1),
            DEFAULT_OVERLAY,
        ));
        finish_mask(room);
    }

    #[test]
    fn a_dialog_inside_the_overlay_window_only_refines_the_hold() {
        let room = 900_005;
        let start = Instant::now();
        assert_leader(&note_mask(room, start, DEFAULT_OVERLAY));
        finish_mask(room);
        // After the danmaku window but inside overlay_second: refine only.
        assert!(matches!(
            note_dialog(
                room,
                start + Duration::from_secs(30),
                ids(&[ReasonId::IdleRecording])
            ),
            PairRole::HoldRefinement
        ));
        // The refinement is one-shot.
        assert!(matches!(
            note_dialog(room, start + Duration::from_secs(31), ids(&[])),
            PairRole::DialogLeader
        ));
    }

    #[test]
    fn a_dialog_after_the_overlay_window_acts_alone() {
        let room = 900_006;
        let start = Instant::now();
        assert_leader(&note_mask(room, start, DEFAULT_OVERLAY));
        finish_mask(room);
        assert!(matches!(
            note_dialog(
                room,
                start + DEFAULT_OVERLAY + Duration::from_secs(1),
                ids(&[])
            ),
            PairRole::DialogLeader
        ));
    }
}
