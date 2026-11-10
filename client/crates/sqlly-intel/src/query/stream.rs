use sqlly_core::event::Event;

pub struct QueryEventStream {
    events: Vec<Event>,
}

impl QueryEventStream {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    pub fn push(&mut self, event: Event) {
        self.events.push(event);
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }
}

impl Default for QueryEventStream {
    fn default() -> Self {
        Self::new()
    }
}
