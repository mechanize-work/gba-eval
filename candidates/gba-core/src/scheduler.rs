//! Event scheduler. Cycle-counted, not per-cycle-stepped — we run the CPU
//! in bursts and fire events when their timestamp is reached.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    HDraw,
    HBlank,
    TimerOverflow(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Scheduled {
    at: u64,
    event: Event,
}

impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at.cmp(&other.at)
    }
}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub struct Scheduler {
    pub now: u64,
    queue: BinaryHeap<Reverse<Scheduled>>,
}

impl Scheduler {
    pub fn new() -> Self {
        let mut s = Self { now: 0, queue: BinaryHeap::new() };
        // Bootstrap PPU: first HBlank at cycle 960 (240 px * 4 cyc).
        s.push(Event::HBlank, 960);
        s
    }

    #[inline]
    pub fn push(&mut self, event: Event, in_cycles: u64) {
        self.queue.push(Reverse(Scheduled { at: self.now + in_cycles, event }));
    }

    #[inline]
    pub fn advance(&mut self, cycles: u32) {
        self.now += cycles as u64;
    }

    #[inline]
    pub fn pop_due(&mut self) -> Option<Event> {
        if let Some(Reverse(s)) = self.queue.peek() {
            if s.at <= self.now {
                return Some(self.queue.pop().unwrap().0.event);
            }
        }
        None
    }

    /// Cycles until next event. CPU uses this to size its run burst.
    #[inline]
    pub fn cycles_until_next(&self) -> u32 {
        self.queue
            .peek()
            .map(|Reverse(s)| (s.at.saturating_sub(self.now)).min(u32::MAX as u64) as u32)
            .unwrap_or(u32::MAX)
    }
}
