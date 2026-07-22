use agent_vocab::TurnId;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

#[derive(Debug)]
pub struct OpenAiCodexSessionState {
    session_id: String,
    window_generation: AtomicU64,
    turn_state: Mutex<Option<CodexTurnState>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexTurnState {
    turn_id: TurnId,
    value: String,
}

impl OpenAiCodexSessionState {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            window_generation: AtomicU64::new(0),
            turn_state: Mutex::new(None),
        }
    }
}

impl OpenAiCodexSessionState {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn window_generation(&self) -> u64 {
        self.window_generation.load(Ordering::Relaxed)
    }

    pub fn set_window_generation(&self, generation: u64) {
        self.window_generation.store(generation, Ordering::Relaxed);
    }

    pub fn observe_transcript_generation(&self, generation: u64) {
        let mut current = self.window_generation();
        while generation > current {
            match self.window_generation.compare_exchange_weak(
                current,
                generation,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn window_id(&self) -> String {
        format!("{}:{}", self.session_id, self.window_generation())
    }

    pub fn turn_state_for_request(&self, turn_id: Option<TurnId>) -> Option<String> {
        let turn_id = turn_id?;
        let guard = self
            .turn_state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard
            .as_ref()
            .filter(|state| state.turn_id == turn_id)
            .map(|state| state.value.clone())
    }

    pub fn record_turn_state(&self, turn_id: Option<TurnId>, value: String) {
        let Some(turn_id) = turn_id else {
            return;
        };
        let mut guard = self
            .turn_state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard = Some(CodexTurnState { turn_id, value });
    }
}
