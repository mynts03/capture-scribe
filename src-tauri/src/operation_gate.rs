//! Short admission lock; the lease, never the mutex, spans media work.
use crate::message::AppMessage;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Capture,
    Selection,
    Processing,
}
#[derive(Default)]
struct Inner {
    serial: u64,
    owner: Option<(u64, Owner)>,
    quitting: bool,
}
#[derive(Clone, Default)]
pub struct OperationGate(Arc<Mutex<Inner>>);
pub struct Lease {
    gate: OperationGate,
    serial: u64,
}
impl OperationGate {
    pub fn acquire(&self, owner: Owner) -> Result<Lease, AppMessage> {
        let mut s = self
            .0
            .lock()
            .map_err(|_| AppMessage::known("operation.busy", "Admission lock poisoned"))?;
        if s.quitting || s.owner.is_some() {
            return Err(AppMessage::known(
                "operation.busy",
                "Another operation or quit request owns admission",
            ));
        }
        s.serial += 1;
        s.owner = Some((s.serial, owner));
        Ok(Lease {
            gate: self.clone(),
            serial: s.serial,
        })
    }
    pub fn owner(&self) -> Option<Owner> {
        self.0.lock().unwrap().owner.map(|x| x.1)
    }
    pub fn quitting(&self, value: bool) {
        self.0.lock().unwrap().quitting = value;
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut s = self.gate.0.lock().unwrap();
        if s.owner.is_some_and(|x| x.0 == self.serial) {
            s.owner = None;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simultaneous_admission_has_one_owner_and_failed_attempts_leave_no_slot() {
        for _ in 0..40 {
            let gate = OperationGate::default();
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let handles: Vec<_> = [Owner::Capture, Owner::Processing]
                .into_iter()
                .map(|owner| {
                    let g = gate.clone();
                    let b = barrier.clone();
                    std::thread::spawn(move || {
                        b.wait();
                        let lease = g.acquire(owner).ok();
                        b.wait();
                        lease
                    })
                })
                .collect();
            barrier.wait();
            barrier.wait();
            let leases: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            assert_eq!(leases.iter().filter(|x| x.is_some()).count(), 1);
            drop(leases);
            assert!(gate.acquire(Owner::Selection).is_ok());
        }
    }
    #[test]
    fn quit_blocks_new_requests_without_releasing_active_owner() {
        let g = OperationGate::default();
        let l = g.acquire(Owner::Processing).unwrap();
        g.quitting(true);
        drop(l);
        assert!(g.acquire(Owner::Capture).is_err());
        g.quitting(false);
        assert!(g.acquire(Owner::Capture).is_ok());
    }
}
