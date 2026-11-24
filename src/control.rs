//! Shared control plane: input remains responsive while model/tools execute.
use std::collections::{HashMap, VecDeque};
use std::sync::{atomic::{AtomicBool, Ordering}, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct Control {
    cancelled: AtomicBool,
    closed: AtomicBool,
    steering: Mutex<VecDeque<String>>,
    approvals: Mutex<HashMap<String, bool>>,
    waiting: Mutex<Option<String>>,
    wake: Condvar,
}
impl Control {
    pub fn cancel(&self) { self.cancelled.store(true, Ordering::Relaxed); self.wake.notify_all(); }
    pub fn close_input(&self) { self.closed.store(true, Ordering::Relaxed); self.wake.notify_all(); }
    pub fn cancelled(&self) -> bool { self.cancelled.load(Ordering::Relaxed) }
    pub fn reset(&self) { self.cancelled.store(false, Ordering::Relaxed); }
    pub fn steer(&self, text: String) { self.steering.lock().expect("steering").push_back(text); }
    pub fn take_steering(&self) -> Vec<String> { self.steering.lock().expect("steering").drain(..).collect() }
    pub fn resolve(&self, id: String, allow: bool) { self.approvals.lock().expect("approvals").insert(id, allow); self.wake.notify_all(); }
    pub fn begin_approval(&self, id: &str) { *self.waiting.lock().expect("waiting")=Some(id.into()); }
    pub fn approval(&self, id: &str) -> Option<bool> {
        *self.waiting.lock().expect("waiting") = Some(id.to_string());
        let mut pending = self.approvals.lock().expect("approvals");
        loop {
            if let Some(allow) = pending.remove(id) { *self.waiting.lock().unwrap()=None; return Some(allow); }
            if self.closed.load(Ordering::Relaxed) || self.cancelled() { *self.waiting.lock().unwrap()=None; return None; }
            pending = self.wake.wait_timeout(pending, Duration::from_millis(100)).expect("approval wait").0;
        }
    }
    pub fn waiting(&self) -> Option<String> { self.waiting.lock().expect("waiting").clone() }
}
