//! Priority definitions and helpers used across all schedulers.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Index, IndexMut};

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

    /// Numeric identifier for compatibility with legacy metrics and IO code.
    ///
    /// Maps priority classes to numeric IDs used in legacy code and external interfaces
    /// (e.g., GUI, metrics serialization). This allows backward compatibility with systems
    /// that expect numeric flow IDs instead of priority enum values.
    ///
    /// # Mapping
    /// - `High` → 1
    /// - `Medium` → 2
    /// - `Low` → 3
    /// - `BestEffort` → 4
    ///
    /// # Returns
    /// Numeric flow ID for this priority (1-4)
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
    ///
    /// Creates a `PriorityTable` by calling the closure once for each priority in the order
    /// defined by `Priority::ALL` (High, Medium, Low, BestEffort). This ensures consistent
    /// ordering regardless of how the table is constructed.
    ///
    /// # Arguments
    /// * `f` - Closure that maps each priority to a value of type `T`
    ///
    /// # Returns
    /// A new `PriorityTable` with values computed by the closure
    ///
    /// # Example
    /// ```
    /// let quantums = PriorityTable::from_fn(|priority| match priority {
    ///     Priority::High => 64,
    ///     Priority::Medium => 8,
    ///     _ => 1,
    /// });
    /// ```
    pub fn from_fn(mut f: impl FnMut(Priority) -> T) -> Self {
        let mut values = Vec::with_capacity(Priority::ALL.len());
        for priority in Priority::ALL {
            values.push(f(priority));
        }
        PriorityTable { values }
    }

    /// Borrow the value for a given priority.
    ///
    /// Returns a reference to the value stored for the specified priority. The value is
    /// indexed using `priority.index()`, which provides O(1) access.
    ///
    /// # Arguments
    /// * `priority` - Priority class to look up
    ///
    /// # Returns
    /// Reference to the value for this priority
    pub fn get(&self, priority: Priority) -> &T {
        &self.values[priority.index()]
    }

    /// Mutably borrow the value for a given priority.
    ///
    /// Returns a mutable reference to the value stored for the specified priority. The value
    /// is indexed using `priority.index()`, which provides O(1) access.
    ///
    /// # Arguments
    /// * `priority` - Priority class to look up
    ///
    /// # Returns
    /// Mutable reference to the value for this priority
    pub fn get_mut(&mut self, priority: Priority) -> &mut T {
        &mut self.values[priority.index()]
    }

    /// Build a table from a vector ordered according to [`Priority::ALL`].
    ///
    /// Creates a `PriorityTable` from a pre-existing vector. The vector must have exactly
    /// `Priority::ALL.len()` elements (currently 4), and they must be in the same order as
    /// `Priority::ALL` (High, Medium, Low, BestEffort).
    ///
    /// # Arguments
    /// * `values` - Vector of values, one per priority in `Priority::ALL` order
    ///
    /// # Returns
    /// A new `PriorityTable` containing the provided values
    ///
    /// # Panics
    /// Panics if `values.len() != Priority::ALL.len()` (assertion failure)
    pub fn from_vec(values: Vec<T>) -> Self {
        assert!(
            values.len() == Priority::ALL.len(),
            "priority table expects {} entries, got {}",
            Priority::ALL.len(),
            values.len()
        );
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
    fn priority_table_builds_and_indexes() {
        let table = PriorityTable::from_fn(|p| p.index());
        assert_eq!(table[Priority::High], 0);
        assert_eq!(table[Priority::BestEffort], 3);
    }
}
