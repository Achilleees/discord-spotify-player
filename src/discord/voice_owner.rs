//! Voice connection ownership shared by joins, departures and delayed audio setup.
//! A retired lease cannot attach a reader or remove a later connection. The async
//! transition lock serializes Discord calls; the short state lock fences effects.

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct VoiceLease {
    pub generation: u64,
    pub channel: u64,
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
        Self::claim_locked(&mut state, channel)
    }

    /// Reserve against the panel revision atomically, before the async join.
    pub fn claim_for(&self, channel: u64, expected: Option<u64>) -> Option<(VoiceLease, u64)> {
        let mut state = self.state.lock();
        if expected.is_some_and(|generation| generation != state.generation) {
            return None;
        }
        let lease = Self::claim_locked(&mut state, channel)?;
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
                state.generation += 1;
            }
        }
    }

    fn claim_locked(state: &mut State, channel: u64) -> Option<VoiceLease> {
        if let Some(lease) = &state.lease {
            return (lease.channel == channel).then(|| lease.clone());
        }
        state.generation += 1;
        let lease = VoiceLease {
            generation: state.generation,
            channel,
            cancelled: CancellationToken::new(),
        };
        state.lease = Some(lease.clone());
        state.connected = false;
        Some(lease)
    }

    pub fn current(&self, lease: &VoiceLease) -> bool {
        self.with_current(lease, || ()).is_some()
    }

    /// Run a synchronous effect while retirement is excluded. Never await or
    /// perform blocking I/O inside the callback.
    pub fn with_current<T>(&self, lease: &VoiceLease, effect: impl FnOnce() -> T) -> Option<T> {
        let state = self.state.lock();
        state
            .lease
            .as_ref()
            .filter(|active| active.generation == lease.generation)?;
        Some(effect())
    }

    /// Invalidate pending setup immediately, before spawning a departure.
    pub fn retire(&self) -> u64 {
        let mut state = self.state.lock();
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
}
