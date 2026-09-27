//! Equal-weight, one-second Bid−Ask samples. Legacy midpoint samples are never imported.
use super::{charts::QuotePoint, strategy::SamplingProgress, Book, Direction};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntryMean {
    pub points: VecDeque<QuotePoint>,
    pub initialized: bool,
}

impl EntryMean {
    pub fn observe(&mut self, books: &[Book; 2], now: u64, max_age: u64, window: u64) -> bool {
        if self.points.back().is_some_and(|p| now / 1000 <= p.time_ms / 1000) { return false; }
        self.points.push_back(QuotePoint::from_books(books, now, max_age));
        while self.points.front().is_some_and(|p| p.time_ms < now.saturating_sub(window)) {
            self.points.pop_front();
        }
        if self.progress(now, window).ready { self.initialized = true; }
        true
    }

    pub fn mean(&self, now: u64, window: u64, direction: Direction) -> Option<Decimal> {
        let mut sum = Decimal::ZERO;
        let mut count = 0u64;
        for p in self.points.iter().filter(|p| p.time_ms <= now && p.time_ms >= now.saturating_sub(window)) {
            if let Some(value) = p.entry_spread(direction) { sum += value; count += 1; }
        }
        (count > 0).then(|| sum / Decimal::from(count))
    }

    pub fn progress(&self, now: u64, window: u64) -> SamplingProgress {
        let start = now.saturating_sub(window);
        let mut covered_ms = 0;
        let mut previous: Option<&QuotePoint> = None;
        let mut latest_valid = None;
        for p in self.points.iter().filter(|p| p.time_ms <= now) {
            if let Some(prev) = previous {
                if prev.entry_spread(Direction::LighterLong).is_some() {
                    covered_ms += p.time_ms.min(prev.time_ms.saturating_add(2000))
                        .saturating_sub(prev.time_ms.max(start));
                }
            }
            if p.entry_spread(Direction::LighterLong).is_some() { latest_valid = Some(p.time_ms); }
            previous = Some(p);
        }
        if let Some(p) = previous {
            if p.entry_spread(Direction::LighterLong).is_some() {
                covered_ms += now.min(p.time_ms.saturating_add(2000)).saturating_sub(p.time_ms.max(start));
            }
        }
        let required_ms = window * if self.initialized { 50 } else { 95 } / 100;
        SamplingProgress {
            continuity_active: false,
            covered_ms,
            required_ms,
            ready: covered_ms >= required_ms && latest_valid.is_some_and(|t| now - t <= 2000),
        }
    }
}
