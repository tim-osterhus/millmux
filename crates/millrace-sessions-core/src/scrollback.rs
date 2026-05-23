use std::{collections::VecDeque, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    error::MillmuxResult,
    storage::{read_json, write_json_atomic},
};

pub const DEFAULT_SCROLLBACK_CAPACITY: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollbackSnapshot {
    pub capacity: usize,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollbackBuffer {
    capacity: usize,
    lines: VecDeque<String>,
}

impl ScrollbackBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            lines: VecDeque::with_capacity(capacity),
        }
    }

    pub fn default_capacity() -> usize {
        DEFAULT_SCROLLBACK_CAPACITY
    }

    pub fn push_line(&mut self, line: impl Into<String>) {
        if self.capacity == 0 {
            return;
        }
        while self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line.into());
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    pub fn snapshot(&self) -> ScrollbackSnapshot {
        ScrollbackSnapshot {
            capacity: self.capacity,
            lines: self.lines(),
        }
    }

    pub fn from_snapshot(snapshot: ScrollbackSnapshot) -> Self {
        let mut buffer = Self::new(snapshot.capacity);
        for line in snapshot.lines {
            buffer.push_line(line);
        }
        buffer
    }

    pub fn persist_snapshot(&self, path: impl AsRef<Path>) -> MillmuxResult<()> {
        write_json_atomic(path, &self.snapshot())
    }

    pub fn restore_snapshot(path: impl AsRef<Path>) -> MillmuxResult<Self> {
        let snapshot: ScrollbackSnapshot = read_json(path)?;
        Ok(Self::from_snapshot(snapshot))
    }
}

impl Default for ScrollbackBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_SCROLLBACK_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_defaults_to_4000_lines() {
        assert_eq!(
            ScrollbackBuffer::default_capacity(),
            DEFAULT_SCROLLBACK_CAPACITY
        );
    }

    #[test]
    fn scrollback_drops_oldest_lines() {
        let mut buffer = ScrollbackBuffer::new(3);
        buffer.push_line("one");
        buffer.push_line("two");
        buffer.push_line("three");
        buffer.push_line("four");
        assert_eq!(buffer.lines(), vec!["two", "three", "four"]);
    }

    #[test]
    fn scrollback_persists_and_restores_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scrollback.snapshot");
        let mut buffer = ScrollbackBuffer::new(2);
        buffer.push_line("a");
        buffer.push_line("b");
        buffer.persist_snapshot(&path).unwrap();
        let restored = ScrollbackBuffer::restore_snapshot(&path).unwrap();
        assert_eq!(restored.lines(), vec!["a", "b"]);
        assert_eq!(restored.snapshot().capacity, 2);
    }
}
