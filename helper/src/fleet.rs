// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! The fibers of one helper, as the control loop tracks them.
//!
//! A fiber is tracked until its thread ends, KILLed or not, so shutdown
//! waits for it. Its fence and endpoint stay its own until it has said
//! EXITED: before that it may still remove its endpoint or write a line for
//! its fence, so a new fiber may take neither. After that it touches
//! neither again, and only its thread lingers.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::fiber::{Cmd, Fiber};

#[derive(Default)]
pub struct Fleet {
    /// Each fence's newest fiber.
    fibers: HashMap<String, Fiber>,
    /// Fibers that said EXITED and whose fence a new fiber took, until
    /// their threads end.
    gone: Vec<Fiber>,
}

impl Fleet {
    pub fn get(&self, fence: &str) -> Option<&Fiber> {
        self.fibers.get(fence)
    }

    /// Forget fibers whose threads have ended.
    pub fn reap(&mut self) {
        self.fibers.retain(|_, f| !f.is_finished());
        self.gone.retain(|f| !f.is_finished());
    }

    /// Make room for a new fiber on `fence` and `endpoint`, or say why
    /// there is none.
    pub fn admit(&mut self, fence: &str, endpoint: &Path) -> Result<(), String> {
        if self.fibers.get(fence).is_some_and(|f| !f.has_ended()) {
            return Err("fence is already a fiber".to_string());
        }
        if let Some(other) = self
            .fibers
            .iter()
            .find_map(|(other, f)| (f.endpoint() == endpoint && !f.has_ended()).then_some(other))
        {
            return Err(format!(
                "endpoint {} is fiber {other}'s, which has not exited",
                endpoint.display()
            ));
        }
        if let Some(old) = self.fibers.remove(fence) {
            self.gone.push(old);
        }
        Ok(())
    }

    /// Track a fiber `admit` made room for.
    pub fn insert(&mut self, fence: String, fiber: Fiber) {
        debug_assert!(!self.fibers.contains_key(&fence));
        self.fibers.insert(fence, fiber);
    }

    /// End every fiber, so each removes its endpoint and its directory, and
    /// wait up to `grace` for their threads, killed and replaced ones too.
    pub fn shutdown(self, grace: Duration) {
        for f in self.fibers.values() {
            f.send(Cmd::Kill);
        }
        let until = Instant::now() + grace;
        let all = || self.fibers.values().chain(&self.gone);
        while Instant::now() < until && all().any(|f| !f.is_finished()) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fiber::testing;
    use std::sync::atomic::Ordering;

    fn wait(what: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !what() {
            assert!(Instant::now() < until, "timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_killed_fiber_keeps_its_fence_and_endpoint_until_it_has_exited() {
        let mut fleet = Fleet::default();
        let ep = Path::new("/run/ep-a");
        let (fiber, stages) = testing::fiber("g/1/1", ep);
        fleet.admit("g/1/1", ep).unwrap();
        fleet.insert("g/1/1".into(), fiber);

        assert!(fleet.get("g/1/1").unwrap().send(Cmd::Kill));
        // Again, and a PARK: a no-op, and refused, never passed on.
        assert!(fleet.get("g/1/1").unwrap().send(Cmd::Kill));
        assert!(!fleet.get("g/1/1").unwrap().send(Cmd::Park {
            dir: "/park".into(),
            sync: true,
        }));
        fleet.reap();
        let err = fleet.admit("g/1/1", Path::new("/run/ep-b")).unwrap_err();
        assert!(err.contains("already a fiber"), "{err}");
        let err = fleet.admit("g/1/2", ep).unwrap_err();
        assert!(err.contains("g/1/1's, which has not exited"), "{err}");

        // EXITED: both are free, though the thread has not ended yet.
        stages.end.send(()).unwrap();
        wait(|| fleet.get("g/1/1").unwrap().has_ended());
        assert_eq!(stages.kills.load(Ordering::SeqCst), 1);
        fleet.admit("g/1/2", ep).unwrap();
        fleet.admit("g/1/1", ep).unwrap();
        assert!(fleet.get("g/1/1").is_none());
        assert_eq!(fleet.gone.len(), 1, "the old thread is no longer tracked");
        fleet.reap();
        assert_eq!(fleet.gone.len(), 1);
        stages.finish.send(()).unwrap();
        wait(|| fleet.gone[0].is_finished());
        fleet.reap();
        assert!(fleet.gone.is_empty());
    }

    #[test]
    fn shutdown_waits_for_killed_and_replaced_fibers() {
        let mut fleet = Fleet::default();
        let (killed, killed_stages) = testing::fiber("g/1/1", Path::new("/a"));
        let (replaced, replaced_stages) = testing::fiber("g/1/2", Path::new("/b"));
        fleet.insert("g/1/1".into(), killed);
        fleet.insert("g/1/2".into(), replaced);
        fleet.get("g/1/1").unwrap().send(Cmd::Kill);
        replaced_stages.end.send(()).unwrap();
        wait(|| fleet.get("g/1/2").unwrap().has_ended());
        fleet.admit("g/1/2", Path::new("/b")).unwrap();

        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            fleet.shutdown(Duration::from_secs(5));
            done.send(()).unwrap();
        });
        killed_stages.end.send(()).unwrap();
        killed_stages.finish.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            finished.try_recv().is_err(),
            "shutdown did not wait for the replaced fiber's thread"
        );
        replaced_stages.finish.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}
