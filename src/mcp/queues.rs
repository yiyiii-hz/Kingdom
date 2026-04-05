use crate::types::{HealthEvent, ManagerNotification};
use std::collections::HashMap;
use tokio::sync::{oneshot, Mutex};

pub struct NotificationQueue {
    pub events: Vec<ManagerNotification>,
}

impl NotificationQueue {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    pub fn push(&mut self, event: ManagerNotification) {
        self.events.push(event);
    }

    pub fn drain(&mut self) -> Vec<ManagerNotification> {
        std::mem::take(&mut self.events)
    }
}

pub struct HealthEventQueue {
    pub events: Vec<HealthEvent>,
}

impl HealthEventQueue {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    pub fn push(&mut self, event: HealthEvent) {
        self.events.push(event);
    }

    pub fn drain(&mut self) -> Vec<HealthEvent> {
        std::mem::take(&mut self.events)
    }
}

pub struct RequestAwaiterRegistry {
    awaiters: HashMap<String, oneshot::Sender<String>>,
}

impl RequestAwaiterRegistry {
    pub fn new() -> Self {
        Self {
            awaiters: HashMap::new(),
        }
    }

    pub fn register(&mut self, request_id: &str) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.awaiters.insert(request_id.to_string(), tx);
        rx
    }

    pub fn signal(&mut self, request_id: &str, answer: String) -> bool {
        if let Some(tx) = self.awaiters.remove(request_id) {
            tx.send(answer).is_ok()
        } else {
            false
        }
    }
}

#[allow(dead_code)]
fn _mutex_reference(_: &Mutex<NotificationQueue>) {}
