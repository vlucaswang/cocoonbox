//! cocoonbox-agent: the in-box agent loop (`serve`) and the tool host (`toolhost`).

use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio_util::sync::CancellationToken;

pub mod bus;
pub mod computer;
pub mod config;
pub mod connect;
pub mod dispatch;
pub mod exec;
pub mod flows;
pub mod identity;
pub mod journal;
pub mod llm;
pub mod mcp;
pub mod offload;
pub mod plugins;
pub mod prompts;
pub mod route;
pub mod runner;
pub mod runs;
pub mod screen;
pub mod secrets;
pub mod server;
pub mod shuttle;
pub mod state;
pub mod store;
pub mod sys;
pub mod toolhost;
pub mod tools;

/// Hands out tokens that one [`Preempt::fire`] cancels together.
#[derive(Default)]
pub struct Preempt(Mutex<CancellationToken>);

impl Preempt {
    /// The token the next [`Preempt::fire`] cancels.
    pub fn token(&self) -> CancellationToken {
        lock(&self.0).clone()
    }

    /// Cancels every token handed out so far; later tokens are fresh.
    pub fn fire(&self) {
        std::mem::take(&mut *lock(&self.0)).cancel();
    }
}

/// Generated types of `cocoonbox.agent.v1`.
#[allow(clippy::all, clippy::pedantic)]
pub mod pb {
    tonic::include_proto!("cocoonbox.agent.v1");
}

/// Current wall-clock time in unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Random identifier with a readable prefix.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
