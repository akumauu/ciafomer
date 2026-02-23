//! Cancellation framework: CancellationToken + generation guard.
//! Ensures stale tasks cannot write results to UI.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

/// Manages task generations. Each new request advances the generation,
/// cancels all prior tasks, and issues a fresh CancellationToken.
pub struct TaskGeneration {
    current_token: RwLock<CancellationToken>,
    generation: AtomicU64,
}

impl TaskGeneration {
    pub fn new() -> Self {
        Self {
            current_token: RwLock::new(CancellationToken::new()),
            generation: AtomicU64::new(0),
        }
    }

    /// Cancel all current tasks, advance generation, return new child token + generation.
    pub fn cancel_and_advance(&self) -> (CancellationToken, u64) {
        let mut token_guard = self.current_token.write();
        token_guard.cancel();
        let new_root = CancellationToken::new();
        let child = new_root.child_token();
        *token_guard = new_root;
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        (child, gen)
    }

    /// Get a child token for the current generation without cancelling.
    pub fn child_token(&self) -> (CancellationToken, u64) {
        let token_guard = self.current_token.read();
        let child = token_guard.child_token();
        let gen = self.generation.load(Ordering::SeqCst);
        (child, gen)
    }

    /// Read current generation.
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Cancel all current tasks without advancing generation.
    pub fn cancel_all(&self) {
        let token_guard = self.current_token.read();
        token_guard.cancel();
    }
}

/// Guard that a task checks before writing results.
/// If the generation has advanced past `my_generation`, the task is stale.
#[derive(Clone)]
pub struct GenerationGuard {
    generation: Arc<AtomicU64>,
    my_generation: u64,
    token: CancellationToken,
}

impl GenerationGuard {
    pub fn new(generation: Arc<AtomicU64>, my_generation: u64, token: CancellationToken) -> Self {
        Self {
            generation,
            my_generation,
            token,
        }
    }

    /// Returns true if this task is still the current generation.
    #[inline]
    pub fn is_current(&self) -> bool {
        self.generation.load(Ordering::SeqCst) == self.my_generation
    }

    /// Returns true if cancellation has been requested.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Returns true if the task should continue (not cancelled and still current).
    #[inline]
    pub fn should_continue(&self) -> bool {
        !self.is_cancelled() && self.is_current()
    }

    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    pub fn my_generation(&self) -> u64 {
        self.my_generation
    }
}

/// Per-pipeline cancellation coordinator.
/// Holds separate TaskGeneration instances for P1 and P2 pipelines.
pub struct CancelCoordinator {
    pub p1: TaskGeneration,
    pub p2: TaskGeneration,
    global_generation: AtomicU64,
}

impl CancelCoordinator {
    pub fn new() -> Self {
        Self {
            p1: TaskGeneration::new(),
            p2: TaskGeneration::new(),
            global_generation: AtomicU64::new(0),
        }
    }

    /// Cancel everything (P1 + P2). Used on new wake event.
    pub fn cancel_all_and_advance(&self) -> u64 {
        self.p1.cancel_and_advance();
        self.p2.cancel_and_advance();
        self.global_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Get a generation guard for P1 tasks.
    pub fn p1_guard(&self) -> GenerationGuard {
        let (token, gen) = self.p1.child_token();
        GenerationGuard::new(
            Arc::new(AtomicU64::new(gen)),
            gen,
            token,
        )
    }

    /// Get a generation guard for P2 tasks.
    pub fn p2_guard(&self) -> GenerationGuard {
        let (token, gen) = self.p2.child_token();
        GenerationGuard::new(
            Arc::new(AtomicU64::new(gen)),
            gen,
            token,
        )
    }
}
