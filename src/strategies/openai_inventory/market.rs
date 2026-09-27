//! Market identity and exact base-quantity conversion. Legacy OPENAI units stay unchanged.
use super::Venue;
use anyhow::{Result, ensure, Context};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketPair { #[default] Openai, Anth }

impl MarketPair {
    pub fn validate_lighter(self, market:&crate::lighter::LighterMarket)->Result<()> {
        use std::str::FromStr;
        ensure!(market.is_active_perp() && market.symbol==self.lighter_symbol()
            && market.market_id==self.lighter_market_id() && market.effective_size_decimals()==self.quantity_decimals()
            && market.effective_price_decimals()==self.lighter_price_decimals()
            && self.units(Decimal::from_str(&market.min_base_amount)?)?==self.minimum_units(Venue::Lighter)
            && Decimal::from_str(&market.min_quote_amount)?==Decimal::from(10),
            "Lighter symbol, ID, precision or minimum order specifications changed");
        Ok(())
    }
    pub fn is_openai(&self) -> bool { *self == Self::Openai }
    pub fn id(self) -> &'static str { match self { Self::Openai => "openai", Self::Anth => "anth" } }
    pub fn label(self) -> &'static str { match self { Self::Openai => "OPENAI", Self::Anth => "ANTH" } }
    pub fn lighter_symbol(self) -> &'static str { match self { Self::Openai => "OPENAI", Self::Anth => "ANTHROPIC" } }
    pub fn lighter_market_id(self) -> i32 { match self { Self::Openai => 42, Self::Anth => 38 } }
    pub fn entropy_symbol(self) -> &'static str { match self { Self::Openai => "io:OAI", Self::Anth => "io:ANTH" } }
    pub fn entropy_margin_mode(self) -> &'static str { match self { Self::Openai => "noCross", Self::Anth => "strictIsolated" } }
    pub fn quantity_decimals(self) -> u32 { match self { Self::Openai => 4, Self::Anth => 5 } }
    pub fn common_step(self) -> i64 { 10_i64.pow(self.quantity_decimals() - 3) }
    pub fn venue_step(self, v: Venue) -> i64 { if v == Venue::Entropy { self.common_step() } else { 1 } }
    pub fn quantity(self, units: i64) -> Decimal { Decimal::new(units, self.quantity_decimals()) }
    pub fn units(self, quantity: Decimal) -> Result<i64> {
        let n = quantity * Decimal::from(10_i64.pow(self.quantity_decimals()));
        ensure!(n.fract().is_zero(), "quantity precision loss for {}", self.label());
        n.to_i64().context("quantity overflow")
    }
    pub fn depth_units(self, quantity: Decimal) -> Result<i64> {
        ensure!(quantity >= Decimal::ZERO, "negative depth");
        (quantity * Decimal::from(10_i64.pow(self.quantity_decimals()))).floor().to_i64().context("depth overflow")
    }
    pub fn common_units(self, notional: Decimal, price: Decimal) -> Result<i64> {
        ensure!(price > Decimal::ZERO, "invalid price");
        (notional / price * Decimal::from(1000)).floor().to_i64()
            .and_then(|n| n.checked_mul(self.common_step())).context("size overflow")
    }
    pub fn lighter_price_decimals(self) -> u32 { match self { Self::Openai => 2, Self::Anth => 1 } }
    pub fn minimum_units(self, v: Venue) -> i64 {
        match (self, v) { (Self::Openai, Venue::Lighter) => 50, (Self::Anth, Venue::Lighter) => 320, _ => self.common_step() }
    }
    pub fn protected_price(self, v: Venue, price: Decimal, buy: bool) -> Result<Decimal> {
        ensure!(price > Decimal::ZERO, "invalid protected price");
        // Lighter uses a fixed tick. Hyperliquid also limits prices to five significant figures.
        let decimals = if v == Venue::Lighter { self.lighter_price_decimals() }
            else { 3_u32.min((5_i32 - price.floor().normalize().to_string().len() as i32).max(0) as u32) };
        let scale = Decimal::from(10_i64.pow(decimals));
        let x = price * scale;
        let rounded = if buy { x.floor() } else { x.ceil() } / scale;
        ensure!(rounded > Decimal::ZERO, "protected price below tick");
        Ok(rounded)
    }
}
