//! Shared control plane: input remains responsive while model/tools execute.
use std::collections::{HashMap, VecDeque};
use std::sync::{atomic::{AtomicU8, Ordering}, Condvar, Mutex};
use std::time::Duration;

const SIG_CANCEL: u8 = 0b001;
const SIG_DETACH: u8 = 0b010;
const SIG_CLOSED: u8 = 0b100;

type ApprovalDecision = (bool, bool, Option<String>);

#[derive(Default)]
pub struct Control {
    signal: AtomicU8,
    steering: Mutex<VecDeque<String>>,
    approvals: Mutex<HashMap<String, ApprovalDecision>>,
    waiting: Mutex<Option<String>>,
    wake: Condvar,
}
impl Control {
    pub fn cancel(&self) { self.signal.fetch_or(SIG_CANCEL, Ordering::Relaxed); self.wake.notify_all(); }
    pub fn request_background(&self) { self.signal.fetch_or(SIG_DETACH, Ordering::Relaxed); self.wake.notify_all(); }
    pub fn take_background_request(&self) -> bool {
        (self.signal.fetch_and(!SIG_DETACH, Ordering::Relaxed) & SIG_DETACH) != 0
    }
    pub fn close_input(&self) { self.signal.fetch_or(SIG_CLOSED, Ordering::Relaxed); self.wake.notify_all(); }
    pub fn cancelled(&self) -> bool { (self.signal.load(Ordering::Relaxed) & SIG_CANCEL) != 0 }
    pub fn reset(&self) {
        self.signal.fetch_and(!(SIG_CANCEL | SIG_DETACH), Ordering::Relaxed);
    }
    pub fn steer(&self, text: String) { self.steering.lock().expect("steering").push_back(text); }
    pub fn take_steering(&self) -> Vec<String> { self.steering.lock().expect("steering").drain(..).collect() }
    pub fn resolve(&self, id: String, allow: bool, remember: bool, note: Option<String>) {
        self.approvals.lock().expect("approvals").insert(id, (allow, remember, note));
        self.wake.notify_all();
    }
    pub fn begin_approval(&self, id: &str) {
        *self.waiting.lock().expect("waiting") = Some(id.into());
    }
    pub fn wait_approval(&self, id: &str) -> Option<(bool, bool, Option<String>)> {
        *self.waiting.lock().expect("waiting") = Some(id.to_string());
        let mut pending = self.approvals.lock().expect("approvals");
        loop {
            if let Some(decision) = pending.remove(id) { *self.waiting.lock().unwrap()=None; return Some(decision); }
            if (self.signal.load(Ordering::Relaxed) & (SIG_CLOSED | SIG_CANCEL)) != 0 {
                *self.waiting.lock().unwrap() = None;
                return None;
            }
            pending = self.wake.wait_timeout(pending, Duration::from_millis(100)).expect("approval wait").0;
        }
    }
    pub fn waiting(&self) -> Option<String> { self.waiting.lock().expect("waiting").clone() }
}
