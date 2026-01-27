use crate::events::Event;
use tokio::sync::broadcast;
use tracing::{debug, warn};

const DEFAULT_CAPACITY: usize = 4096;

/// Typed event bus built on tokio broadcast channels.
///
/// All events are broadcast to all subscribers. Subscribers can filter
/// by topic at receive time using topic-filtered subscriptions.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publish an event to all subscribers.
    pub fn publish(&self, event: Event) {
        let topic = event.topic();
        match self.sender.send(event) {
            Ok(receivers) => {
                debug!(topic, receivers, "event published");
            }
            Err(_) => {
                warn!(topic, "event published but no active subscribers");
            }
        }
    }

    /// Create a new subscriber that receives all events.
    pub fn subscribe(&self) -> EventSubscriber {
        EventSubscriber {
            receiver: self.sender.subscribe(),
            topics: None,
        }
    }

    /// Create a subscriber filtered to specific topics.
    pub fn subscribe_topics(&self, topics: &[&str]) -> EventSubscriber {
        EventSubscriber {
            receiver: self.sender.subscribe(),
            topics: Some(topics.iter().map(|t| t.to_string()).collect()),
        }
    }

    /// Number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

pub struct EventSubscriber {
    receiver: broadcast::Receiver<Event>,
    topics: Option<Vec<String>>,
}

impl EventSubscriber {
    /// Consume the subscriber and return the raw broadcast receiver.
    /// Useful for wrapping in `BroadcastStream` for SSE endpoints.
    pub fn into_receiver(self) -> broadcast::Receiver<Event> {
        self.receiver
    }
}

impl EventSubscriber {
    /// Receive the next event, respecting topic filter.
    /// Returns None if the channel is closed.
    pub async fn recv(&mut self) -> Option<Event> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => {
                    if let Some(ref topics) = self.topics
                        && !topics.iter().any(|t| t == event.topic())
                    {
                        continue;
                    }
                    return Some(event);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "event subscriber lagged, skipped events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return None;
                }
            }
        }
    }
}
