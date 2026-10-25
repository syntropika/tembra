//! Optional bounded metadata collection. Payloads and exporters stay outside the kernel.
use crate::{Error, Id, Result};
use std::collections::VecDeque;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub operation: &'static str,
    pub owner: Id,
    pub started: u64,
    pub finished: u64,
    pub error: Option<&'static str>,
}

pub struct Telemetry {
    capacity: usize,
    sample_every: u64,
    seen: u64,
    dropped: u64,
    events: VecDeque<Observation>,
}

impl Telemetry {
    pub fn new(capacity: usize, sample_every: u64) -> Result<Self> {
        if capacity == 0 || sample_every == 0 {
            return Err(Error::invalid(
                "telemetry",
                "capacity and sampling interval must be positive",
            ));
        }
        Ok(Self {
            capacity,
            sample_every,
            seen: 0,
            dropped: 0,
            events: VecDeque::new(),
        })
    }
    pub fn record(&mut self, observation: Observation) {
        self.seen = self.seen.wrapping_add(1);
        if !self.seen.is_multiple_of(self.sample_every) {
            return;
        }
        if self.events.len() == self.capacity {
            self.events.pop_front();
            self.dropped += 1;
        }
        self.events.push_back(observation);
    }
    pub fn drain(&mut self, limit: usize) -> Vec<Observation> {
        let count = limit.min(self.events.len());
        self.events.drain(..count).collect()
    }
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}
