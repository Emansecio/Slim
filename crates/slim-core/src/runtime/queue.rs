use std::collections::VecDeque;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptQueue {
    capacity: usize,
    items: VecDeque<String>,
}

impl PromptQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            items: VecDeque::new(),
        }
    }

    pub fn push(&mut self, prompt: impl Into<String>) -> Result<(), &'static str> {
        if self.items.len() >= self.capacity {
            return Err("prompt queue full");
        }
        self.items.push_back(prompt.into());
        Ok(())
    }

    pub fn pop(&mut self) -> Option<String> {
        self.items.pop_front()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
