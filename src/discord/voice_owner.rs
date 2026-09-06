//! Voice connection ownership shared by joins, departures and delayed audio setup.
//! A retired lease cannot attach a reader or remove a later connection. The async
//! transition lock serializes Discord calls; the short state lock fences effects.

use crate::routing::VoiceActivity;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct VoiceLease {
    pub generation: u64,
    pub channel: u64,
    pub activity: VoiceActivity,
    pub requester: Option<u64>,
    pub cancelled: CancellationToken,
}

#[derive(Default)]
struct State {
    generation: u64,
    lease: Option<VoiceLease>,
    connected: bool,
}

#[derive(Default)]
pub(super) struct VoiceOwner {
    state: Mutex<State>,
    pub transitions: tokio::sync::Mutex<()>,
}

impl VoiceOwner {
    /// Reuse this room's lease or reserve an idle connection. A different room
    /// must release its ownership before this process can serve another room.
    #[cfg(test)]
    pub fn claim(&self, channel: u64) -> Option<VoiceLease> {
        let mut state = self.state.lock();
        Self::claim_locked(&mut state, channel, false)
    }

    /// Reserve against the panel revision atomically, before the async join.
    pub fn claim_for(&self, channel: u64, expected: Option<u64>) -> Option<(VoiceLease, u64)> {
        let mut state = self.state.lock();
        if expected.is_some_and(|generation| generation != state.generation) {
            return None;
        }
        // Only an already-authorized/automatic music claim may preempt a visit.
        let lease = Self::claim_locked(&mut state, channel, expected.is_none())?;
        Some((lease, state.generation))
    }

    /// An administrator moving the existing call changes the routing revision,
    /// but keeps the same connection/reader lease alive. Old panels expire.
    pub fn observe_channel(&self, channel: u64) {
        let mut state = self.state.lock();
        // A pending replacement may still receive the previous join's echo.
        if !state.connected {
            return;
        }
        if let Some(lease) = &mut state.lease {
            if lease.channel != channel {
                lease.channel = channel;
                if lease.activity == VoiceActivity::Soundboard {
                    lease.cancelled.cancel();
                }
                state.generation += 1;
            }
        }
    }

    fn claim_locked(state: &mut State, channel: u64, preempt_visit: bool) -> Option<VoiceLease> {
        if let Some(lease) = &state.lease {
            if lease.activity == VoiceActivity::Music {
                return (lease.channel == channel && !lease.cancelled.is_cancelled())
                    .then(|| lease.clone());
            }
            if !preempt_visit {
                return None;
            }
            lease.cancelled.cancel();
        }
        state.generation += 1;
        let lease = VoiceLease {
            generation: state.generation,
            channel,
            activity: VoiceActivity::Music,
            requester: None,
            cancelled: CancellationToken::new(),
        };
        state.lease = Some(lease.clone());
        state.connected = false;
        Some(lease)
    }

    pub fn claim_visit(&self, channel: u64, user: u64, expected: u64) -> Option<VoiceLease> {
        let mut state = self.state.lock();
        if state.generation != expected || state.lease.is_some() {
            return None;
        }
        state.generation += 1;
        let lease = VoiceLease {
            generation: state.generation,
            channel,
            activity: VoiceActivity::Soundboard,
            requester: Some(user),
            cancelled: CancellationToken::new(),
        };
        state.lease = Some(lease.clone());
        state.connected = false;
        Some(lease)
    }

    pub fn activity(&self) -> Option<VoiceActivity> {
        self.state.lock().lease.as_ref().map(|lease| lease.activity)
    }

    pub fn music_room(&self) -> Option<u64> {
        self.state
            .lock()
            .lease
            .as_ref()
            .filter(|lease| lease.activity == VoiceActivity::Music)
            .map(|lease| lease.channel)
    }

    pub fn cancel_visit(&self) {
        if let Some(lease) = self
            .state
            .lock()
            .lease
            .as_ref()
            .filter(|lease| lease.activity == VoiceActivity::Soundboard)
        {
            lease.cancelled.cancel();
        }
    }

    pub fn requester_moved(&self, user: u64, channel: Option<u64>) {
        if let Some(lease) = self
            .state
            .lock()
            .lease
            .as_ref()
            .filter(|lease| lease.requester == Some(user))
        {
            if channel != Some(lease.channel) {
                lease.cancelled.cancel();
            }
        }
    }

    /// A cancelled visit still owns cleanup until retired or superseded.
    pub fn owns(&self, lease: &VoiceLease) -> bool {
        self.state
            .lock()
            .lease
            .as_ref()
            .is_some_and(|active| active.generation == lease.generation)
    }

    pub fn retire_if(&self, lease: &VoiceLease) -> bool {
        let mut state = self.state.lock();
        if state
            .lease
            .as_ref()
            .is_none_or(|active| active.generation != lease.generation)
        {
            return false;
        }
        Self::retire_locked(&mut state);
        true
    }

    pub fn retire_music(&self) -> Option<u64> {
        self.retire_music_checked(None)
    }

    pub fn retire_music_if(&self, expected: u64) -> Option<u64> {
        self.retire_music_checked(Some(expected))
    }

    fn retire_music_checked(&self, expected: Option<u64>) -> Option<u64> {
        let mut state = self.state.lock();
        if expected.is_some_and(|generation| state.generation != generation) {
            return None;
        }
        if state
            .lease
            .as_ref()
            .is_some_and(|lease| lease.activity != VoiceActivity::Music)
        {
            return None;
        }
        Some(Self::retire_locked(&mut state))
    }

    pub fn current(&self, lease: &VoiceLease) -> bool {
        self.with_current(lease, || ()).is_some()
    }

    /// Run a synchronous effect while retirement is excluded. Never await or
    /// perform blocking I/O inside the callback.
    pub fn with_current<T>(&self, lease: &VoiceLease, effect: impl FnOnce() -> T) -> Option<T> {
        let state = self.state.lock();
        state.lease.as_ref().filter(|active| {
            active.generation == lease.generation && !active.cancelled.is_cancelled()
        })?;
        Some(effect())
    }

    /// Invalidate pending setup immediately, before spawning a departure.
    #[cfg(test)]
    pub fn retire(&self) -> u64 {
        Self::retire_locked(&mut self.state.lock())
    }

    fn retire_locked(state: &mut State) -> u64 {
        if let Some(lease) = state.lease.take() {
            lease.cancelled.cancel();
        }
        state.generation += 1;
        state.generation
    }

    pub fn retirement_current(&self, generation: u64) -> bool {
        let state = self.state.lock();
        state.generation == generation && state.lease.is_none()
    }

    pub fn connected(&self, lease: &VoiceLease) -> bool {
        let state = self.state.lock();
        state.connected
            && state
                .lease
                .as_ref()
                .is_some_and(|active| active.generation == lease.generation)
    }

    pub fn mark_connected(&self, lease: &VoiceLease) -> bool {
        let mut state = self.state.lock();
        if lease.cancelled.is_cancelled() {
            return false;
        }
        if state
            .lease
            .as_ref()
            .is_none_or(|active| active.generation != lease.generation)
        {
            return false;
        }
        state.connected = true;
        true
    }

    pub fn failed(&self, lease: &VoiceLease) {
        let mut state = self.state.lock();
        if state
            .lease
            .as_ref()
            .is_some_and(|active| active.generation == lease.generation)
        {
            lease.cancelled.cancel();
            state.lease = None;
            state.generation += 1;
        }
    }

    pub fn status(&self) -> (u64, Option<u64>, Option<VoiceActivity>) {
        let state = self.state.lock();
        (
            state.generation,
            state.lease.as_ref().map(|lease| lease.channel),
            state.lease.as_ref().map(|lease| lease.activity),
        )
    }

    pub fn snapshot(&self) -> (u64, Option<u64>) {
        let state = self.state.lock();
        (
            state.generation,
            state.lease.as_ref().map(|lease| lease.channel),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_room_cannot_take_a_pending_join() {
        let owner = VoiceOwner::default();
        let lease = owner.claim(10).unwrap();
        assert!(owner.claim(20).is_none());
        assert_eq!(owner.claim(10).unwrap().generation, lease.generation);
    }

    #[test]
    fn stop_cancels_delayed_reader_and_departure_cannot_remove_replacement() {
        let owner = VoiceOwner::default();
        let old = owner.claim(10).unwrap();
        let departure = owner.retire();
        assert!(old.cancelled.is_cancelled());
        assert!(owner
            .with_current(&old, || panic!("stale reader attached"))
            .is_none());
        assert!(owner.retirement_current(departure));
        let replacement = owner.claim(20).unwrap();
        assert!(!owner.retirement_current(departure));
        owner.failed(&old);
        assert!(owner.current(&replacement));
    }

    #[test]
    fn same_room_rejoin_gets_a_new_generation() {
        let owner = VoiceOwner::default();
        let old = owner.claim(10).unwrap();
        owner.retire();
        let new = owner.claim(10).unwrap();
        assert_ne!(old.generation, new.generation);
        assert!(!owner.current(&old));
        assert_eq!(owner.snapshot(), (new.generation, Some(10)));
    }
    #[test]
    fn stopped_login_cannot_claim_voice_and_admin_move_expires_panels() {
        let owner = VoiceOwner::default();
        let initial = owner.snapshot().0;
        owner.retire();
        assert!(owner.claim_for(10, Some(initial)).is_none());
        let (lease, revision) = owner.claim_for(10, None).unwrap();
        owner.observe_channel(99); // Stale gateway echo while replacement joins.
        assert_eq!(owner.snapshot(), (revision, Some(10)));
        owner.mark_connected(&lease);
        owner.observe_channel(20);
        assert_ne!(owner.snapshot().0, revision);
        assert_eq!(owner.snapshot().1, Some(20));
        assert!(owner.current(&lease));
        assert!(owner.connected(&lease));
        assert!(!lease.cancelled.is_cancelled());
        assert!(owner.claim_for(10, Some(revision)).is_none());
        let (_, moved_revision) = owner.claim_for(20, None).unwrap();
        assert_eq!(moved_revision, owner.snapshot().0);
    }

    #[test]
    fn music_ownership_blocks_visits_before_join_and_while_paused() {
        let owner = VoiceOwner::default();
        let music = owner.claim(10).unwrap();
        for connected in [false, true] {
            if connected {
                // Pausing audio does not release the connection lease.
                assert!(owner.mark_connected(&music));
            }
            let revision = owner.snapshot().0;
            assert!(owner.claim_visit(10, 100, revision).is_none());
            assert!(owner.claim_visit(20, 100, revision).is_none());
            assert!(owner.current(&music));
            assert_eq!(owner.music_room(), Some(10));
        }
    }

    #[test]
    fn visit_rejects_duplicate_visits_and_guarded_music_even_in_same_room() {
        let owner = VoiceOwner::default();
        let visit = owner.claim_visit(10, 100, owner.snapshot().0).unwrap();
        let revision = owner.snapshot().0;
        for room in [10, 20] {
            assert!(owner.claim_visit(room, 100, revision).is_none());
            assert!(owner.claim_visit(room, 200, revision).is_none());
            assert!(owner.claim_for(room, Some(revision)).is_none());
        }
        assert_eq!(owner.activity(), Some(VoiceActivity::Soundboard));
        assert_eq!(owner.music_room(), None);
        assert!(owner.current(&visit));
        assert_eq!(owner.snapshot(), (revision, Some(10)));
    }

    #[test]
    fn automatic_music_preempts_a_visit_without_inheriting_its_room() {
        let owner = VoiceOwner::default();
        let visit = owner.claim_visit(10, 100, owner.snapshot().0).unwrap();
        assert!(owner.mark_connected(&visit));
        let (music, revision) = owner.claim_for(20, None).unwrap();

        assert!(visit.cancelled.is_cancelled());
        assert!(!owner.owns(&visit));
        assert!(!owner.current(&visit));
        assert!(owner.current(&music));
        assert!(!owner.connected(&music));
        assert_eq!(owner.snapshot(), (revision, Some(20)));
        assert_eq!(owner.activity(), Some(VoiceActivity::Music));
        assert_eq!(owner.music_room(), Some(20));

        // A queued clip completion/failure cannot release its successor.
        assert!(!owner.retire_if(&visit));
        owner.failed(&visit);
        assert!(!owner.mark_connected(&visit));
        assert!(owner.current(&music));
    }

    #[test]
    fn music_departure_cannot_retire_a_visit() {
        let owner = VoiceOwner::default();
        let music = owner.claim(10).unwrap();
        let departure = owner.retire_music().unwrap();
        assert!(music.cancelled.is_cancelled());
        assert!(owner.retirement_current(departure));
        let visit = owner.claim_visit(20, 100, departure).unwrap();

        assert!(!owner.retirement_current(departure));
        assert_eq!(owner.retire_music(), None);
        assert!(owner.current(&visit));
        assert_eq!(owner.activity(), Some(VoiceActivity::Soundboard));
    }

    #[test]
    fn cancelled_visit_still_owns_cleanup_but_cannot_install_audio() {
        let owner = VoiceOwner::default();
        let visit = owner.claim_visit(10, 100, owner.snapshot().0).unwrap();
        owner.cancel_visit();

        assert!(visit.cancelled.is_cancelled());
        assert!(owner.owns(&visit));
        assert!(!owner.current(&visit));
        assert!(owner
            .with_current(&visit, || panic!("cancelled clip installed audio"))
            .is_none());
        assert!(owner.claim_visit(10, 200, owner.snapshot().0).is_none());
        assert!(owner.retire_if(&visit));
        assert!(!owner.owns(&visit));
        assert_eq!(owner.activity(), None);
    }

    #[test]
    fn only_requester_departure_cancels_a_visit_and_reentry_does_not_revive_it() {
        for destination in [None, Some(20)] {
            let owner = VoiceOwner::default();
            let visit = owner.claim_visit(10, 100, owner.snapshot().0).unwrap();
            owner.requester_moved(200, destination);
            owner.requester_moved(100, Some(10));
            assert!(owner.current(&visit));

            owner.requester_moved(100, destination);
            assert!(visit.cancelled.is_cancelled());
            assert!(owner.owns(&visit));
            owner.requester_moved(100, Some(10));
            assert!(!owner.current(&visit));
        }
    }

    #[test]
    fn admin_move_cancels_visit_but_preserves_its_cleanup_ownership() {
        let owner = VoiceOwner::default();
        let visit = owner.claim_visit(10, 100, owner.snapshot().0).unwrap();
        let initial_revision = owner.snapshot().0;
        owner.observe_channel(99); // Old gateway echo while joining.
        assert_eq!(owner.snapshot(), (initial_revision, Some(10)));
        assert!(owner.current(&visit));
        assert!(owner.mark_connected(&visit));
        owner.observe_channel(10);
        assert!(owner.current(&visit));
        owner.observe_channel(20);

        assert!(visit.cancelled.is_cancelled());
        assert!(owner.owns(&visit));
        assert!(!owner.current(&visit));
        assert!(owner.snapshot().0 > initial_revision);
        assert_eq!(owner.snapshot().1, Some(20));
        assert!(owner.retire_if(&visit));
    }

    #[test]
    fn stale_visit_admission_and_cleanup_cannot_replace_a_new_visit() {
        let owner = VoiceOwner::default();
        let idle_revision = owner.snapshot().0;
        let old = owner.claim_visit(10, 100, idle_revision).unwrap();
        assert!(owner.retire_if(&old));
        assert!(owner.claim_visit(10, 100, idle_revision).is_none());
        let next = owner.claim_visit(10, 200, owner.snapshot().0).unwrap();

        assert!(!owner.retire_if(&old));
        owner.failed(&old);
        owner.requester_moved(100, None);
        assert!(owner.current(&next));
        assert_eq!(owner.snapshot().1, Some(10));
    }
    #[test]
    fn old_disconnect_cannot_retire_replacement_music() {
        let owner = VoiceOwner::default();
        let (_, revision) = owner.claim_for(10, None).unwrap();
        owner.retire_music();
        let (new, _) = owner.claim_for(20, None).unwrap();
        assert!(owner.retire_music_if(revision).is_none());
        assert!(owner.current(&new));
    }
}
