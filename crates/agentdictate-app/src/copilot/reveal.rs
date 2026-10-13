//! Holds an answer drafted ahead of confirmation until it is wanted.
//!
//! An automatic answer starts as soon as the other side's question is
//! transcribed, while the fast model is still judging whether the question is
//! meant for the user. Its events wait here, hidden, and reach the window only
//! once the question is confirmed; a rejected draft is discarded unseen.

use std::sync::{Mutex, PoisonError};

use agentdictate_core::CopilotEvent;

use super::engine::UiSink;

enum State {
    Hidden(Vec<CopilotEvent>),
    Shown,
    Discarded,
}

pub(crate) struct RevealGate {
    ui: UiSink,
    state: Mutex<State>,
}

impl RevealGate {
    /// A gate that passes every event straight to the window.
    pub(crate) fn shown(ui: UiSink) -> Self {
        Self {
            ui,
            state: Mutex::new(State::Shown),
        }
    }

    /// A gate that holds events until [`Self::reveal`].
    pub(crate) fn hidden(ui: UiSink) -> Self {
        Self {
            ui,
            state: Mutex::new(State::Hidden(Vec::new())),
        }
    }

    pub(crate) fn send(&self, event: CopilotEvent) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *state {
            State::Hidden(held) => held.push(event),
            // Sending under the lock keeps order with a concurrent reveal.
            State::Shown => (self.ui)(event),
            State::Discarded => {}
        }
    }

    /// Shows everything held so far, in order, then passes later events
    /// through. Returns false if the draft was already discarded.
    pub(crate) fn reveal(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match std::mem::replace(&mut *state, State::Shown) {
            State::Hidden(held) => {
                for event in held {
                    (self.ui)(event);
                }
                true
            }
            State::Shown => true,
            State::Discarded => {
                *state = State::Discarded;
                false
            }
        }
    }

    /// Throws away a hidden draft. Returns false if it is already shown.
    pub(crate) fn discard(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &*state {
            State::Shown => false,
            State::Hidden(_) | State::Discarded => {
                *state = State::Discarded;
                true
            }
        }
    }

    pub(crate) fn is_hidden(&self) -> bool {
        matches!(
            *self.state.lock().unwrap_or_else(PoisonError::into_inner),
            State::Hidden(_)
        )
    }

    pub(crate) fn is_discarded(&self) -> bool {
        matches!(
            *self.state.lock().unwrap_or_else(PoisonError::into_inner),
            State::Discarded
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn recorder() -> (UiSink, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let ui: UiSink = Arc::new(move |event| {
            if let CopilotEvent::Notice(text) = event {
                sink_seen.lock().unwrap().push(text);
            }
        });
        (ui, seen)
    }

    fn notice(text: &str) -> CopilotEvent {
        CopilotEvent::Notice(text.into())
    }

    #[test]
    fn hidden_drafts_appear_in_order_once_revealed() {
        let (ui, seen) = recorder();
        let gate = RevealGate::hidden(ui);
        gate.send(notice("started"));
        gate.send(notice("first words"));
        assert!(seen.lock().unwrap().is_empty());
        assert!(gate.is_hidden());
        assert!(gate.reveal());
        gate.send(notice("more words"));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["started", "first words", "more words"]
        );
        // A shown answer cannot be withdrawn.
        assert!(!gate.discard());
    }

    #[test]
    fn discarded_drafts_are_never_shown() {
        let (ui, seen) = recorder();
        let gate = RevealGate::hidden(ui);
        gate.send(notice("started"));
        assert!(gate.discard());
        gate.send(notice("late words"));
        assert!(!gate.reveal());
        assert!(gate.is_discarded());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn shown_gates_pass_events_straight_through() {
        let (ui, seen) = recorder();
        let gate = RevealGate::shown(ui);
        gate.send(notice("now"));
        assert_eq!(*seen.lock().unwrap(), vec!["now"]);
    }
}
