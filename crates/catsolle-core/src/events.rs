use crate::session::SessionState;
use crate::transfer::TransferProgress;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub enum Event {
    SessionStateChanged {
        session_id: Uuid,
        state: SessionState,
    },
    TransferProgress {
        job_id: Uuid,
        progress: TransferProgress,
    },
    Notification {
        level: String,
        message: String,
    },
}

#[derive(Clone, Debug)]
pub struct EventBus {
    sender: tokio::sync::broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(capacity);
        Self { sender }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    pub fn send(&self, event: Event) {
        let _ = self.sender.send(event);
    }
}
