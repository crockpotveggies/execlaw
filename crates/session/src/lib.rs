//! execlaw-session
//!
//! A `Session` binds a conversation to the runner and to whichever pipeline
//! components are required by its modality (text → `runner-local`; voice →
//! `voice-pipeline` wrapping `runner-local`). Phase 0 ships only a type
//! outline; the state machine lands with Phase 1.

#![forbid(unsafe_code)]

use execlaw_core::ConversationId;
use execlaw_core::conversation::Modality;

/// A handle to an in-flight conversation's plumbing.
#[derive(Debug, Clone)]
pub struct Session {
    pub conversation_id: ConversationId,
    pub modality: Modality,
}

impl Session {
    pub fn new(conversation_id: ConversationId, modality: Modality) -> Self {
        Self {
            conversation_id,
            modality,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_binds_modality() {
        let s = Session::new(ConversationId::from("c"), Modality::Voice);
        assert_eq!(s.modality, Modality::Voice);
    }
}
