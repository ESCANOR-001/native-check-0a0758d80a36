//! Bounded reconnect intent for owned Claude hosts. Retries attach a saved
//! identity only; they never resend prompts or authorize native controls.
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub(crate) const HOST_LIMIT: usize = 8;
#[derive(Default)]
pub(crate) struct Recovery {
    slots: HashMap<String, Slot>,
}
struct Slot {
    due: Option<Instant>,
    connected: Option<Instant>,
    failures: u32,
}
impl Recovery {
    pub fn remember(&mut self, id: &str, now: Instant) {
        if self.slots.len() < HOST_LIMIT {
            self.slots.entry(id.into()).or_insert(Slot {
                due: Some(now),
                connected: None,
                failures: 0,
            });
        }
    }
    pub fn connected(&mut self, id: &str, now: Instant) {
        self.remember(id, now);
        if let Some(slot) = self.slots.get_mut(id) {
            slot.due = None;
            slot.connected = Some(now);
        }
    }
    pub fn failed(&mut self, id: &str, now: Instant) {
        self.remember(id, now);
        if let Some(slot) = self.slots.get_mut(id) {
            // A handshake followed immediately by another failure is still
            // the same outage. Avoid a fast reconnect loop on auth failures.
            if slot
                .connected
                .take()
                .is_some_and(|at| now.duration_since(at) >= Duration::from_secs(30))
            {
                slot.failures = 0;
            }
            let delay = (2u64 << slot.failures.min(6)).min(120);
            slot.failures = slot.failures.saturating_add(1);
            slot.due = Some(now + Duration::from_secs(delay));
        }
    }
    pub fn ready(&self, now: Instant) -> Vec<String> {
        let mut ready: Vec<_> = self
            .slots
            .iter()
            .filter_map(|(id, slot)| slot.due.filter(|at| *at <= now).map(|at| (at, id.clone())))
            .collect();
        ready.sort();
        ready.into_iter().map(|(_, id)| id).collect()
    }
    pub fn forget(&mut self, id: &str) {
        self.slots.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retries_are_bounded_back_off_and_reset_only_after_a_stable_connection() {
        let mut r = Recovery::default();
        let now = Instant::now();
        for n in 0..20 {
            r.remember(&n.to_string(), now);
        }
        assert_eq!(r.ready(now).len(), HOST_LIMIT);
        r.failed("0", now);
        assert!(!r.ready(now + Duration::from_secs(1)).contains(&"0".into()));
        assert!(r.ready(now + Duration::from_secs(2)).contains(&"0".into()));
        r.connected("0", now + Duration::from_secs(2));
        r.failed("0", now + Duration::from_secs(3));
        assert!(!r.ready(now + Duration::from_secs(6)).contains(&"0".into()));
        assert!(r.ready(now + Duration::from_secs(7)).contains(&"0".into()));
        r.connected("0", now + Duration::from_secs(7));
        r.failed("0", now + Duration::from_secs(40));
        assert!(r.ready(now + Duration::from_secs(42)).contains(&"0".into()));
        r.forget("0");
        assert!(!r
            .ready(now + Duration::from_secs(1000))
            .contains(&"0".into()));
    }
}
