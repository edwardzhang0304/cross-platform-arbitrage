use anyhow::{Result, ensure};
use chrono::{Datelike, Duration, TimeZone, Timelike, Utc, Weekday};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    External,
    Internal,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PricingRegime {
    Ee,
    Ie,
    Ei,
    Ii,
    Unknown,
}

impl PricingRegime {
    pub fn derive(lighter: PriceSource, trade: PriceSource) -> Self {
        match (lighter, trade) {
            (PriceSource::External, PriceSource::External) => Self::Ee,
            (PriceSource::Internal, PriceSource::External) => Self::Ie,
            (PriceSource::External, PriceSource::Internal) => Self::Ei,
            (PriceSource::Internal, PriceSource::Internal) => Self::Ii,
            _ => Self::Unknown,
        }
    }

    pub fn is_mixed(self) -> bool {
        matches!(self, Self::Ie | Self::Ei)
    }

    pub fn permits_new_v1_position(self) -> bool {
        matches!(self, Self::Ee | Self::Ie | Self::Ei)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusinessSession {
    UsRth,
    UsPremarket,
    UsAfterhours,
    UsOvernight,
    WeekendInternal,
    HolidayInternal,
    Transition,
}

/// Classifies an already-normalized US/Eastern wall clock. Pricing source is
/// supplied by the venue adapter; this function never infers it from the clock.
/// Weekday follows chrono: Monday=1 ... Sunday=7.
pub fn classify_us_session(weekday: u8, minute: u16, trade_external: bool) -> BusinessSession {
    if !trade_external {
        return BusinessSession::WeekendInternal;
    }
    match weekday {
        1..=5 if minute < 4 * 60 => BusinessSession::UsOvernight,
        1..=5 if minute < 9 * 60 + 30 => BusinessSession::UsPremarket,
        1..=5 if minute < 16 * 60 => BusinessSession::UsRth,
        1..=5 if minute < 20 * 60 => BusinessSession::UsAfterhours,
        1..=4 => BusinessSession::UsOvernight,
        7 if minute >= 20 * 60 => BusinessSession::UsOvernight,
        _ => BusinessSession::WeekendInternal,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomaticTradeClock {
    pub source: PriceSource,
    pub session: BusinessSession,
    pub eastern_weekday: u8,
    pub eastern_minute: u16,
    pub daylight_saving: bool,
}

/// Conservative 24/5 HIP-3 stock-session clock classifier. This resolves the documented
/// weekly session and US/Eastern DST boundary, but does not invent holiday
/// metadata. A future authoritative holiday/session flag may override it.
pub fn automatic_trade_clock(timestamp_ms: u64) -> Result<AutomaticTradeClock> {
    let timestamp_ms = i64::try_from(timestamp_ms)
        .map_err(|_| anyhow::anyhow!("trade clock timestamp exceeds the supported chrono range"))?;
    let utc = chrono::DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .ok_or_else(|| anyhow::anyhow!("invalid trade clock timestamp"))?;
    let year = utc.year();
    let dst_start_day = nth_weekday_of_month(year, 3, Weekday::Sun, 2)?;
    let dst_end_day = nth_weekday_of_month(year, 11, Weekday::Sun, 1)?;
    // US DST changes at 02:00 local: 07:00 UTC in March, 06:00 UTC in November.
    let dst_start = Utc
        .with_ymd_and_hms(year, 3, dst_start_day, 7, 0, 0)
        .single()
        .ok_or_else(|| anyhow::anyhow!("invalid US DST start"))?;
    let dst_end = Utc
        .with_ymd_and_hms(year, 11, dst_end_day, 6, 0, 0)
        .single()
        .ok_or_else(|| anyhow::anyhow!("invalid US DST end"))?;
    let daylight_saving = utc >= dst_start && utc < dst_end;
    let eastern = utc + Duration::hours(if daylight_saving { -4 } else { -5 });
    let weekday = eastern.weekday().number_from_monday() as u8;
    let minute = (eastern.hour() * 60 + eastern.minute()) as u16;
    let trade_external = matches!(weekday, 1..=4)
        || (weekday == 5 && minute < 20 * 60)
        || (weekday == 7 && minute >= 20 * 60);
    Ok(AutomaticTradeClock {
        source: if trade_external {
            PriceSource::External
        } else {
            PriceSource::Internal
        },
        session: classify_us_session(weekday, minute, trade_external),
        eastern_weekday: weekday,
        eastern_minute: minute,
        daylight_saving,
    })
}

fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, nth: u32) -> Result<u32> {
    ensure!((1..=5).contains(&nth), "weekday occurrence must be 1..=5");
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1)
        .ok_or_else(|| anyhow::anyhow!("invalid calendar month"))?;
    let offset = (7 + weekday.num_days_from_monday() as i64
        - first.weekday().num_days_from_monday() as i64)
        % 7;
    let day = 1 + offset as u32 + (nth - 1) * 7;
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| anyhow::anyhow!("weekday occurrence falls outside month"))?;
    Ok(day)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BookSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderBook {
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    pub exchange_timestamp_ms: u64,
    pub received_timestamp_ms: u64,
    pub sequence_valid: bool,
}

impl OrderBook {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.sequence_valid, "order book sequence is invalid");
        ensure!(
            !self.bids.is_empty() && !self.asks.is_empty(),
            "two-sided book required"
        );
        for level in self.bids.iter().chain(&self.asks) {
            ensure!(
                level.price.is_finite() && level.price > 0.0,
                "book price must be positive"
            );
            ensure!(
                level.size.is_finite() && level.size > 0.0,
                "book size must be positive"
            );
        }
        ensure!(
            self.bids.windows(2).all(|w| w[0].price >= w[1].price),
            "bids must be descending"
        );
        ensure!(
            self.asks.windows(2).all(|w| w[0].price <= w[1].price),
            "asks must be ascending"
        );
        ensure!(self.bids[0].price < self.asks[0].price, "book is crossed");
        Ok(())
    }

    pub fn mid(&self) -> Result<f64> {
        self.validate()?;
        Ok((self.bids[0].price + self.asks[0].price) / 2.0)
    }

    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.received_timestamp_ms)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExecutionEstimate {
    pub vwap: f64,
    pub filled_size: f64,
    pub slippage_bp: f64,
    pub levels_consumed: usize,
    pub complete: bool,
}

pub fn executable_vwap(
    book: &OrderBook,
    side: BookSide,
    target_size: f64,
) -> Result<ExecutionEstimate> {
    book.validate()?;
    ensure!(
        target_size.is_finite() && target_size > 0.0,
        "target size must be positive"
    );
    let levels = match side {
        BookSide::Buy => &book.asks,
        BookSide::Sell => &book.bids,
    };
    let touch = levels[0].price;
    let mut remaining = target_size;
    let mut notional = 0.0;
    let mut consumed = 0;
    for level in levels {
        if remaining <= 1e-12 {
            break;
        }
        let take = remaining.min(level.size);
        notional += take * level.price;
        remaining -= take;
        consumed += 1;
    }
    let filled = target_size - remaining;
    ensure!(filled > 0.0, "book has no executable liquidity");
    let vwap = notional / filled;
    let raw_slippage = match side {
        BookSide::Buy => (vwap - touch) / touch * 10_000.0,
        BookSide::Sell => (touch - vwap) / touch * 10_000.0,
    };
    Ok(ExecutionEstimate {
        vwap,
        filled_size: filled,
        slippage_bp: raw_slippage.max(0.0),
        levels_consumed: consumed,
        complete: remaining <= 1e-9,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> OrderBook {
        OrderBook {
            bids: vec![
                PriceLevel {
                    price: 99.0,
                    size: 2.0,
                },
                PriceLevel {
                    price: 98.0,
                    size: 3.0,
                },
            ],
            asks: vec![
                PriceLevel {
                    price: 101.0,
                    size: 2.0,
                },
                PriceLevel {
                    price: 102.0,
                    size: 3.0,
                },
            ],
            exchange_timestamp_ms: 1,
            received_timestamp_ms: 2,
            sequence_valid: true,
        }
    }

    #[test]
    fn vwap_consumes_real_depth_and_reports_shortfall() {
        let complete = executable_vwap(&book(), BookSide::Buy, 4.0).unwrap();
        assert_eq!(complete.vwap, 101.5);
        assert!(complete.complete);
        let short = executable_vwap(&book(), BookSide::Sell, 6.0).unwrap();
        assert_eq!(short.filled_size, 5.0);
        assert!(!short.complete);
    }

    #[test]
    fn overnight_is_supported_but_weekend_internal_is_not() {
        assert_eq!(
            classify_us_session(2, 60, true),
            BusinessSession::UsOvernight
        );
        assert_eq!(
            classify_us_session(7, 21 * 60, true),
            BusinessSession::UsOvernight
        );
        assert_eq!(
            classify_us_session(6, 12 * 60, false),
            BusinessSession::WeekendInternal
        );
    }

    #[test]
    fn unknown_regime_fails_closed() {
        let regime = PricingRegime::derive(PriceSource::External, PriceSource::Unknown);
        assert_eq!(regime, PricingRegime::Unknown);
        assert!(!regime.permits_new_v1_position());
    }

    #[test]
    fn automatic_trade_clock_handles_dst_and_weekend_boundary() {
        let summer = Utc.with_ymd_and_hms(2026, 8, 25, 14, 0, 0).unwrap();
        let summer = automatic_trade_clock(summer.timestamp_millis() as u64).unwrap();
        assert!(summer.daylight_saving);
        assert_eq!(summer.eastern_minute, 10 * 60);
        assert_eq!(summer.session, BusinessSession::UsRth);
        assert_eq!(summer.source, PriceSource::External);

        let saturday = Utc.with_ymd_and_hms(2026, 8, 29, 16, 0, 0).unwrap();
        let saturday = automatic_trade_clock(saturday.timestamp_millis() as u64).unwrap();
        assert_eq!(saturday.source, PriceSource::Internal);
        assert_eq!(saturday.session, BusinessSession::WeekendInternal);

        let winter = Utc.with_ymd_and_hms(2026, 12, 1, 15, 0, 0).unwrap();
        let winter = automatic_trade_clock(winter.timestamp_millis() as u64).unwrap();
        assert!(!winter.daylight_saving);
        assert_eq!(winter.eastern_minute, 10 * 60);
    }
}
