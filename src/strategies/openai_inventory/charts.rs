//! Shared raw quotes for the live chart and the opening-spread moving average.
use super::{Book, Direction};
use rust_decimal::Decimal;
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotePoint {
    pub time_ms: u64,
    pub lighter_bid: Option<Decimal>,
    pub lighter_ask: Option<Decimal>,
    pub entropy_bid: Option<Decimal>,
    pub entropy_ask: Option<Decimal>,
}

impl QuotePoint {
    pub fn from_books(books: &[Book; 2], now: u64, max_age: u64) -> Self {
        let quote = |book: &Book| {
            if book.validate(now, max_age).is_ok() {
                (Some(book.bids[0].price), Some(book.asks[0].price))
            } else {
                (None, None)
            }
        };
        let (lighter_bid, lighter_ask) = quote(&books[0]);
        let (entropy_bid, entropy_ask) = quote(&books[1]);
        Self { time_ms: now, lighter_bid, lighter_ask, entropy_bid, entropy_ask }
    }

    pub fn entry_spread(&self, direction: Direction) -> Option<Decimal> {
        // Both venue quotes must be valid; a missing side is a gap, never zero.
        let (lb, la, eb, ea) = (self.lighter_bid?, self.lighter_ask?, self.entropy_bid?, self.entropy_ask?);
        if lb <= Decimal::ZERO || eb <= Decimal::ZERO || la < lb || ea < eb { return None; }
        Some(match direction {
            Direction::LighterLong => eb - la,
            Direction::LighterShort => lb - ea,
        })
    }
}
