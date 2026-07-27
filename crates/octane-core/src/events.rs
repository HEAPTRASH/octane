//! The core's outbound event channel.
//!
//! The loop publishes [`Event`]s as they happen rather than returning a
//! transcript at the end. That is what makes streaming possible at all: a
//! `run()` that only returns when the turn is over cannot show a token before
//! the turn is over.
//!
//! Unbounded on purpose. A bounded channel would make the agent loop wait on the
//! UI, so a slow repaint would throttle inference — the wrong dependency
//! direction. The volume is text deltas, which is small.

use octane_protocol::{Event, Item, ItemEvent, ItemId, ItemKind, ItemStatus, TurnId};

/// Where the loop publishes to.
#[derive(Debug, Clone)]
pub struct EventSink {
    sender: tokio::sync::mpsc::UnboundedSender<Event>,
    turn: TurnId,
}

impl EventSink {
    pub fn new(turn: TurnId) -> (Self, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (Self { sender, turn }, receiver)
    }

    /// A sink that discards everything, for tests and headless runs.
    pub fn discard() -> Self {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        // Leaked deliberately: dropping the receiver would make every send fail
        // and turn "no subscriber" into an error path the caller must handle.
        std::mem::forget(receiver);
        Self { sender, turn: TurnId::new() }
    }

    /// Send an event. Failure means nobody is listening, which is not an error.
    pub fn send(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    /// Announce an item and return its id, so deltas can address it.
    pub fn item_started(&self, kind: ItemKind) -> ItemId {
        let id = ItemId::new();
        self.send(Event::Item(ItemEvent::Started {
            turn_id: self.turn.clone(),
            item: Item { id: id.clone(), kind, status: ItemStatus::Streaming },
        }));
        id
    }

    pub fn item_delta(&self, item: &ItemId, text: impl Into<String>) {
        self.send(Event::Item(ItemEvent::Delta {
            turn_id: self.turn.clone(),
            item_id: item.clone(),
            text: text.into(),
        }));
    }

    /// Finish an item with its terminal payload.
    pub fn item_completed(&self, id: ItemId, kind: ItemKind, status: ItemStatus) {
        self.send(Event::Item(ItemEvent::Completed {
            turn_id: self.turn.clone(),
            item: Item { id, kind, status },
        }));
    }

    /// Publish a whole item that never streamed.
    pub fn item(&self, kind: ItemKind) {
        self.item_completed(ItemId::new(), kind, ItemStatus::Completed);
    }

    pub fn usage(&self, usage: octane_protocol::Usage) {
        self.send(Event::Usage(usage));
    }

    pub fn turn_status(&self, status: octane_protocol::TurnStatus) {
        self.send(Event::Turn(octane_protocol::TurnEvent {
            thread_id: octane_protocol::ThreadId::new(),
            turn_id: self.turn.clone(),
            status,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_streams_started_delta_completed() {
        let (sink, mut events) = EventSink::new(TurnId::new());

        let id = sink.item_started(ItemKind::AgentMessage { text: String::new() });
        sink.item_delta(&id, "hel");
        sink.item_delta(&id, "lo");
        sink.item_completed(
            id.clone(),
            ItemKind::AgentMessage { text: "hello".into() },
            ItemStatus::Completed,
        );

        let mut received = Vec::new();
        while let Ok(event) = events.try_recv() {
            received.push(event);
        }
        assert_eq!(received.len(), 4);

        // Deltas must address the item they belong to, or a client cannot tell
        // which of several in-flight items they extend.
        assert!(matches!(
            &received[1],
            Event::Item(ItemEvent::Delta { item_id, text, .. }) if *item_id == id && text == "hel"
        ));
        assert!(matches!(&received[3], Event::Item(ItemEvent::Completed { .. })));
    }

    #[test]
    fn every_event_in_a_turn_carries_the_same_turn_id() {
        let turn = TurnId::new();
        let (sink, mut events) = EventSink::new(turn.clone());

        sink.item(ItemKind::AgentMessage { text: "x".into() });
        sink.turn_status(octane_protocol::TurnStatus::Completed);

        while let Ok(event) = events.try_recv() {
            match event {
                Event::Item(ItemEvent::Completed { turn_id, .. }) => assert_eq!(turn_id, turn),
                Event::Turn(turn_event) => assert_eq!(turn_event.turn_id, turn),
                _ => {}
            }
        }
    }

    #[test]
    fn sending_without_a_subscriber_is_not_an_error() {
        // The loop must not care whether a UI is attached.
        let sink = EventSink::discard();
        sink.item(ItemKind::AgentMessage { text: "nobody is listening".into() });
    }

    #[test]
    fn a_dropped_receiver_does_not_break_the_loop() {
        let (sink, events) = EventSink::new(TurnId::new());
        drop(events);
        sink.item(ItemKind::AgentMessage { text: "still fine".into() });
    }
}
