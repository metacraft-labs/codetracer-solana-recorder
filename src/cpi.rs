//! CPI (Cross-Program Invocation) boundary detection.
//!
//! When a Solana program invokes another program via CPI, the program
//! counter jumps to a completely different address range belonging to
//! the target program's `.text` section. This module detects those
//! boundaries by monitoring the PC register across consecutive
//! register snapshots.

use std::ops::Range;

use crate::register_trace::RegisterSnapshot;

/// Events emitted by [`CpiDetector`] when processing register snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CpiEvent {
    /// The PC remains within the current program's address range.
    SameProgram,
    /// The PC jumped outside the current program's range, indicating
    /// a cross-program invocation.
    CpiCall {
        /// The PC value in the target program.
        target_pc: u64,
    },
    /// The PC returned to the previous program's range after a CPI.
    CpiReturn {
        /// The PC value we returned to.
        return_pc: u64,
    },
}

/// Tracks nested CPI context: which program we are in and at what depth.
#[derive(Debug, Clone)]
pub struct CpiContext {
    /// Name or identifier of the program at this level.
    pub program_name: String,
    /// The PC range for this program.
    pub pc_range: Range<u64>,
}

/// Detects CPI boundaries by monitoring the program counter across
/// consecutive register snapshots.
///
/// The detector is initialised with the primary program's expected PC
/// range. When the PC jumps outside that range it emits a
/// [`CpiEvent::CpiCall`]; when it returns it emits a
/// [`CpiEvent::CpiReturn`].
#[derive(Debug)]
pub struct CpiDetector {
    /// Stack of CPI contexts. The bottom entry is the primary program.
    context_stack: Vec<CpiContext>,
    /// All known program ranges for matching CPI targets.
    known_ranges: Vec<(String, Range<u64>)>,
}

impl CpiDetector {
    /// Create a new detector for a program whose PCs fall in `primary_range`.
    pub fn new(primary_range: Range<u64>) -> Self {
        let ctx = CpiContext {
            program_name: "primary".to_string(),
            pc_range: primary_range.clone(),
        };
        Self {
            context_stack: vec![ctx],
            known_ranges: vec![("primary".to_string(), primary_range)],
        }
    }

    /// Register an additional program range that may be invoked via CPI.
    pub fn add_program_range(&mut self, name: &str, range: Range<u64>) {
        self.known_ranges.push((name.to_string(), range));
    }

    /// Process a single register snapshot and return the CPI event.
    pub fn process_snapshot(&mut self, snapshot: &RegisterSnapshot) -> CpiEvent {
        let pc = snapshot.pc();
        let current = self
            .context_stack
            .last()
            .expect("context stack must never be empty");

        if current.pc_range.contains(&pc) {
            return CpiEvent::SameProgram;
        }

        // Check if we are returning to a previous context on the stack.
        // Walk from the second-to-last entry backwards.
        if self.context_stack.len() > 1 {
            for i in (0..self.context_stack.len() - 1).rev() {
                if self.context_stack[i].pc_range.contains(&pc) {
                    // Pop everything above this level.
                    self.context_stack.truncate(i + 1);
                    return CpiEvent::CpiReturn { return_pc: pc };
                }
            }
        }

        // PC is outside all stacked contexts -- this is a CPI call.
        // Try to match a known program range.
        let name = self
            .known_ranges
            .iter()
            .find(|(_, r)| r.contains(&pc))
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| format!("unknown_at_{pc}"));

        let range = self
            .known_ranges
            .iter()
            .find(|(_, r)| r.contains(&pc))
            .map(|(_, r)| r.clone())
            .unwrap_or(pc..pc + 1);

        self.context_stack.push(CpiContext {
            program_name: name,
            pc_range: range,
        });

        CpiEvent::CpiCall { target_pc: pc }
    }

    /// Current CPI nesting depth (0 = primary program, 1 = first CPI, etc.).
    pub fn call_depth(&self) -> usize {
        self.context_stack.len() - 1
    }

    /// Whether execution is currently inside a CPI (not in the primary program).
    pub fn is_in_cpi(&self) -> bool {
        self.context_stack.len() > 1
    }

    /// The name of the program currently being executed.
    pub fn current_program(&self) -> &str {
        &self
            .context_stack
            .last()
            .expect("context stack must never be empty")
            .program_name
    }
}
