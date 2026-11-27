//! The producer side of a turn's event stream (D-002).

use tokio::sync::mpsc;

use crate::{Event, EventBody, TurnEvents};

/// Sends one turn's events. It never blocks: the channel is unbounded, so a consumer
/// that stops reading cannot wedge notification routing, approvals, interrupt or close.
/// It delivers exactly one terminal event and then closes the stream.
#[derive(Debug)]
pub struct TurnSender {
    tx: Option<mpsc::UnboundedSender<Event>>,
}

impl TurnSender {
    /// A new stream and its sender.
    #[must_use]
    pub fn new() -> (Self, TurnEvents) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx: Some(tx) }, rx)
    }

    /// Sends a non-terminal event; dropped after the turn finished or if the consumer
    /// is gone. A terminal body is treated as [`TurnSender::finish`].
    pub fn emit(&mut self, body: EventBody) {
        if body.is_terminal() {
            let _ended = self.finish(body);
            return;
        }
        if let Some(tx) = &self.tx {
            // The consumer may have stopped listening; the event then has no reader.
            drop(tx.send(Event::now(body)));
        }
    }

    /// Sends the terminal event once and closes the stream. Returns whether this call
    /// ended the turn.
    pub fn finish(&mut self, body: EventBody) -> bool {
        match self.tx.take() {
            Some(tx) => {
                drop(tx.send(Event::now(body)));
                true
            }
            None => false,
        }
    }

    /// Whether the terminal event was sent.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.tx.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::TurnSender;
    use crate::{EventBody, Failure, FailureKind};

    #[tokio::test]
    async fn order_terminal_and_close() {
        let (mut tx, mut rx) = TurnSender::new();
        tx.emit(EventBody::Text("a".into()));
        tx.emit(EventBody::Text("b".into()));
        assert!(tx.finish(EventBody::TurnFailed(Failure::new(
            FailureKind::Interrupted,
            "x"
        ))));
        assert!(
            !tx.finish(EventBody::TurnFailed(Failure::new(
                FailureKind::Crashed,
                "late"
            ))),
            "second terminal"
        );
        tx.emit(EventBody::Text("after".into()));
        let mut got = Vec::new();
        while let Some(e) = rx.recv().await {
            got.push(e.body);
        }
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], EventBody::Text("a".into()));
        assert!(got[2].is_terminal());
        assert!(tx.is_done());
    }

    #[test]
    fn producer_never_blocks() {
        let (mut tx, rx) = TurnSender::new();
        for i in 0..100_000 {
            tx.emit(EventBody::Text(i.to_string()));
        }
        drop(rx);
        tx.emit(EventBody::Text("consumer gone".into()));
        assert!(tx.finish(EventBody::TurnFailed(Failure::new(
            FailureKind::Unknown,
            "end"
        ))));
    }
}
