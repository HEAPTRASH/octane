//! Loop detection.
//!
//! Catches the classic failure where a model re-reads the same file, or re-runs a
//! failing command, indefinitely. A step cap alone does not help: 1000 steps of
//! the same `read` is 1000 paid round trips before anything stops it.
//!
//! The approach is Crush's (`internal/agent/loop_detection.go`): hash
//! `(tool, input, output)` per step and abort when any signature recurs too often
//! in a sliding window. Cheap, and it needs no understanding of what the tools do.
//!
//! Including the **output** in the signature is what makes it correct. Repeating a
//! call whose output *changed* is progress — polling a build, tailing a log, or
//! re-running a test after an edit. Only a repeat with identical output is a loop.

use std::collections::HashMap;
use std::collections::VecDeque;

/// Steps considered.
pub const DEFAULT_WINDOW: usize = 10;

/// Repeats within the window that constitute a loop.
pub const DEFAULT_MAX_REPEATS: usize = 5;

#[derive(Debug, Clone)]
pub struct LoopGuard {
    window: usize,
    max_repeats: usize,
    signatures: VecDeque<String>,
}

impl Default for LoopGuard {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW, DEFAULT_MAX_REPEATS)
    }
}

impl LoopGuard {
    pub fn new(window: usize, max_repeats: usize) -> Self {
        Self { window, max_repeats, signatures: VecDeque::with_capacity(window) }
    }

    /// Record a completed step's tool interactions.
    ///
    /// A step with no tool calls records nothing: pure reasoning steps are not
    /// candidates for this kind of loop, and counting them would let a long
    /// chain of thought push real signatures out of the window.
    pub fn record(&mut self, interactions: &[(String, String, String)]) {
        if interactions.is_empty() {
            return;
        }

        let signature = signature_of(interactions);
        self.signatures.push_back(signature);
        while self.signatures.len() > self.window {
            self.signatures.pop_front();
        }
    }

    /// The offending signature and its count, if the window shows a loop.
    pub fn detect(&self) -> Option<(String, usize)> {
        if self.signatures.len() < self.window {
            return None;
        }

        let mut counts: HashMap<&String, usize> = HashMap::new();
        for signature in &self.signatures {
            let count = counts.entry(signature).or_default();
            *count += 1;
            if *count > self.max_repeats {
                return Some((signature.clone(), *count));
            }
        }
        None
    }

    pub fn reset(&mut self) {
        self.signatures.clear();
    }
}

/// Order-independent digest of a step's tool interactions.
///
/// Sorted before hashing so two steps that issued the same calls in a different
/// order — which parallel tool calls make routine — compare equal.
fn signature_of(interactions: &[(String, String, String)]) -> String {
    let mut parts: Vec<String> = interactions
        .iter()
        .map(|(tool, input, output)| format!("{tool}\u{0}{input}\u{0}{output}"))
        .collect();
    parts.sort();

    // Not cryptographic; this only needs to be a cheap equality key.
    let joined = parts.join("\u{1}");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in joined.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(tool: &str, input: &str, output: &str) -> Vec<(String, String, String)> {
        vec![(tool.into(), input.into(), output.into())]
    }

    #[test]
    fn quiet_until_the_window_is_full() {
        let mut guard = LoopGuard::new(10, 5);
        for _ in 0..9 {
            guard.record(&call("read", "a.rs", "contents"));
        }
        // Nine identical steps, but the window is not yet full.
        assert!(guard.detect().is_none());
    }

    #[test]
    fn detects_a_stuck_repeat() {
        let mut guard = LoopGuard::new(10, 5);
        for _ in 0..10 {
            guard.record(&call("read", "a.rs", "contents"));
        }
        let (_, repeats) = guard.detect().expect("should detect a loop");
        assert!(repeats > 5);
    }

    #[test]
    fn changing_output_is_progress_not_a_loop() {
        let mut guard = LoopGuard::new(10, 5);
        // Polling a build: same call, different output each time.
        for i in 0..10 {
            guard.record(&call("bash", "cargo build", &format!("compiling crate {i}")));
        }
        assert!(guard.detect().is_none(), "identical calls with differing output are progress");
    }

    #[test]
    fn varied_work_is_not_a_loop() {
        let mut guard = LoopGuard::new(10, 5);
        for i in 0..10 {
            guard.record(&call("read", &format!("file{i}.rs"), "contents"));
        }
        assert!(guard.detect().is_none());
    }

    #[test]
    fn parallel_call_order_does_not_matter() {
        let forward = vec![
            ("read".to_string(), "a".to_string(), "x".to_string()),
            ("read".to_string(), "b".to_string(), "y".to_string()),
        ];
        let reversed: Vec<_> = forward.iter().rev().cloned().collect();
        assert_eq!(signature_of(&forward), signature_of(&reversed));
    }

    #[test]
    fn reasoning_only_steps_do_not_fill_the_window() {
        let mut guard = LoopGuard::new(10, 5);
        for _ in 0..20 {
            guard.record(&[]);
        }
        assert!(guard.detect().is_none());
    }

    #[test]
    fn reset_clears_history() {
        let mut guard = LoopGuard::new(10, 5);
        for _ in 0..10 {
            guard.record(&call("read", "a.rs", "contents"));
        }
        assert!(guard.detect().is_some());
        guard.reset();
        assert!(guard.detect().is_none());
    }
}
