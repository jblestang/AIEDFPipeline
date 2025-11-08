//! Core scheduling primitives shared across the ingress/egress DRR stages and the EDF processor.
//! 
//! The module defines the global [`Priority`] ordering used throughout the pipeline, the
//! [`Packet`] representation that flows between stages, and the [`PriorityTable`] helper that
//! keeps APIs stable when new priority classes are introduced.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Index, IndexMut};
use std::time::{Duration, Instant};

/// System-wide latency/priority classes ordered from most to least critical.
///
/// The ordering is stable so schedulers can rely on integer indexes instead of branching on
/// specific labels. Adding a new class only requires appending it to [`Priority::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Priority {
    High,
    Medium,
    Low,
    BestEffort,
}

impl Priority {
    /// Ordered list of all priorities (high → low) for iteration utilities.
    pub const ALL: [Priority; 4] = [
        Priority::High,
        Priority::Medium,
        Priority::Low,
        Priority::BestEffort,
    ];

    /// Stable index for priority based arrays.
    pub const fn index(self) -> usize {
        match self {
            Priority::High => 0,
            Priority::Medium => 1,
            Priority::Low => 2,
            Priority::BestEffort => 3,
        }
    }

    /// Map a legacy numeric flow identifier to the associated priority class.
    ///
    /// This is retained for backwards compatibility with external workloads that still encode
    /// priorities as integers on the wire.
    #[allow(dead_code)]
    pub const fn from_flow_id(flow_id: u64) -> Priority {
        match flow_id {
            1 => Priority::High,
            2 => Priority::Medium,
            3 => Priority::Low,
            4 => Priority::BestEffort,
            _ => Priority::BestEffort,
        }
    }

    /// Numeric identifier for compatibility with legacy metrics and IO code.
    #[allow(dead_code)]
    pub const fn flow_id(self) -> u64 {
        match self {
            Priority::High => 1,
            Priority::Medium => 2,
            Priority::Low => 3,
            Priority::BestEffort => 4,
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Priority::High => "high",
            Priority::Medium => "medium",
            Priority::Low => "low",
            Priority::BestEffort => "best_effort",
        };
        write!(f, "{label}")
    }
}

/// Lightweight representation of a work unit travelling through the pipeline.
///
/// Each [`Packet`] captures payload bytes, their latency budget, and the [`Priority`] used by the
/// schedulers. The timestamp is filled when the packet enters the pipeline so downstream stages can
/// compute latency and deadline misses.
#[derive(Debug, Clone)]
pub struct Packet {
    #[cfg_attr(not(test), allow(dead_code))]
    pub flow_id: u64,
    pub data: Vec<u8>,
    pub timestamp: Instant,
    pub latency_budget: Duration,
    pub priority: Priority,
}

impl Packet {
    /// Create a packet while ensuring the flow/priorities are consistent.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(priority: Priority, data: Vec<u8>, latency_budget: Duration) -> Packet {
        Packet {
            flow_id: priority.flow_id(),
            priority,
            timestamp: Instant::now(),
            latency_budget,
            data,
        }
    }
}

/// Helper structure wrapping a value per [`Priority`].
///
/// This allows APIs to remain stable when new priorities are introduced: as long as
/// [`Priority::ALL`] is updated, the table automatically grows and all call sites iterate
/// dynamically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorityTable<T> {
    values: Vec<T>,
}

impl<T> PriorityTable<T> {
    /// Build a table by executing a closure for each priority.
    pub fn from_fn(mut f: impl FnMut(Priority) -> T) -> Self {
        let mut values = Vec::with_capacity(Priority::ALL.len());
        for priority in Priority::ALL {
            values.push(f(priority));
        }
        PriorityTable { values }
    }

    /// Try to build a table while propagating errors from the generator.
    #[allow(dead_code)]
    pub fn try_from_fn<E>(mut f: impl FnMut(Priority) -> Result<T, E>) -> Result<Self, E> {
        let mut values = Vec::with_capacity(Priority::ALL.len());
        for priority in Priority::ALL {
            values.push(f(priority)?);
        }
        Ok(PriorityTable { values })
    }

    /// Borrow the value for a given priority.
    pub fn get(&self, priority: Priority) -> &T {
        &self.values[priority.index()]
    }

    /// Mutably borrow the value for a given priority.
    pub fn get_mut(&mut self, priority: Priority) -> &mut T {
        &mut self.values[priority.index()]
    }

    /// Consume the table and return the underlying vector.
    #[allow(dead_code)]
    pub fn into_vec(self) -> Vec<T> {
        self.values
    }

    /// Build a table from a vector ordered according to [`Priority::ALL`].
    pub fn from_vec(values: Vec<T>) -> Self {
        assert!(
            values.len() == Priority::ALL.len(),
            "priority table expects {} entries, got {}",
            Priority::ALL.len(),
            values.len()
        );
        PriorityTable { values }
    }

    /// Iterate over `(Priority, &T)` pairs.
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = (Priority, &T)> {
        Priority::ALL.iter().copied().zip(self.values.iter())
    }

    /// Iterate over `(Priority, &mut T)` pairs.
    #[allow(dead_code)]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Priority, &mut T)> {
        Priority::ALL.iter().copied().zip(self.values.iter_mut())
    }

    /// Map the values into a new `PriorityTable`.
    #[allow(dead_code)]
    pub fn map<U>(self, mut f: impl FnMut(Priority, T) -> U) -> PriorityTable<U> {
        let mut values = Vec::with_capacity(self.values.len());
        for (idx, value) in self.values.into_iter().enumerate() {
            let priority = Priority::ALL[idx];
            values.push(f(priority, value));
        }
        PriorityTable { values }
    }
}

impl<T> Index<Priority> for PriorityTable<T> {
    type Output = T;

    fn index(&self, index: Priority) -> &Self::Output {
        self.get(index)
    }
}

impl<T> IndexMut<Priority> for PriorityTable<T> {
    fn index_mut(&mut self, index: Priority) -> &mut Self::Output {
        self.get_mut(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_index_is_stable() {
        assert_eq!(Priority::High.index(), 0);
        assert_eq!(Priority::Medium.index(), 1);
        assert_eq!(Priority::Low.index(), 2);
        assert_eq!(Priority::BestEffort.index(), 3);
    }

    #[test]
    fn packet_builder_sets_priority() {
        let p = Packet::new(Priority::Medium, vec![1, 2, 3], Duration::from_millis(10));
        assert_eq!(p.priority, Priority::Medium);
        assert_eq!(p.flow_id, Priority::Medium.flow_id());
    }

    #[test]
    fn priority_table_builds_and_indexes() {
        let table = PriorityTable::from_fn(|p| p.index());
        assert_eq!(table[Priority::High], 0);
        assert_eq!(table[Priority::BestEffort], 3);

        let mapped = table
            .clone()
            .map(|priority, value| value + priority.index());
        assert_eq!(mapped[Priority::Medium], 2);
    }
}
