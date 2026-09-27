//! Public settlement rates applied to virtual positions. Hyperliquid oracle history is
//! not available here: previous-hour public candle close is an explicit price proxy.
//! These are simulation estimates, never reported as real account payments.
use super::*;
use anyhow::{Context, Result, ensure};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
pub const HOUR:u64=3_600_000;
fn dec(v:&Value)->Result<Decimal> { Decimal::from_str(v.as_str().context("missing public funding decimal")?).context("invalid public funding decimal") }

pub async fn per_base(venue:Venue,market:MarketPair,start:u64,end:u64)->Result<Vec<(u64,Decimal)>> {
    ensure!(start<=end && end-start<=720*HOUR,"funding query outside bounded history range");
    let client=reqwest::Client::builder().timeout(std::time::Duration::from_secs(4)).build()?;
    let mut result=Vec::new();
    if venue==Venue::Lighter {
        let v:Value=client.get("https://api.rh.lighter.xyz/api/v1/fundings")
            .query(&[("market_id",market.lighter_market_id().to_string()),("resolution","1h".into()),
                // Lighter uses an exclusive end and counts that boundary in count_back.
                ("start_timestamp",(start/1000).to_string()),("end_timestamp",(end.checked_add(HOUR).context("funding range overflow")?/1000).to_string()),("count_back",((end-start)/HOUR+2).to_string())])
            .send().await?.error_for_status()?.json().await?;
        ensure!(v["code"]==200,"public funding request failed");
        for row in v["fundings"].as_array().context("missing public funding rows")? {
            let time=row["timestamp"].as_u64().context("invalid funding timestamp")?*1000;
            if time<start||time>end {continue;}
            let value=dec(&row["value"])?;
            ensure!(value>=Decimal::ZERO,"invalid funding value");
            let sign=match row["direction"].as_str() {Some("long")=>Decimal::ONE,Some("short")=>-Decimal::ONE,_=>anyhow::bail!("unknown funding payer")};
            result.push((time,sign*value));
        }
    } else {
        let query_end=end.checked_add(HOUR-1).context("funding range overflow")?;
        let rates:Value=client.post("https://api.hyperliquid.xyz/info").json(&json!({"type":"fundingHistory","coin":market.entropy_symbol(),"startTime":start,"endTime":query_end}))
            .send().await?.error_for_status()?.json().await?;
        let candles:Value=client.post("https://api.hyperliquid.xyz/info").json(&json!({"type":"candleSnapshot","req":{"coin":market.entropy_symbol(),"interval":"1h","startTime":start.saturating_sub(HOUR),"endTime":end}}))
            .send().await?.error_for_status()?.json().await?;
        let candles=candles.as_array().context("missing public funding price proxy")?;
        for row in rates.as_array().context("missing public funding rates")? {
            ensure!(row["coin"].as_str()==Some(market.entropy_symbol()),"wrong funding market");
            // Public settlements carry millisecond publication offsets; paper accounting
            // uses the corresponding hourly boundary consistently across both venues.
            let time=row["time"].as_u64().context("invalid funding timestamp")?/HOUR*HOUR;
            if time<start||time>end {continue;}
            let candle=candles.iter().find(|c|c["t"].as_u64()==Some(time.saturating_sub(HOUR)))
                .context("settlement price proxy unavailable")?;
            ensure!(candle["s"].as_str()==Some(market.entropy_symbol()),"wrong funding price market");
            let price=dec(&candle["c"])?; let rate=dec(&row["fundingRate"])?;
            ensure!(price>Decimal::ZERO && rate.abs()<=Decimal::new(1,2),"invalid funding price/rate");
            result.push((time,price*rate));
        }
    }
    result.sort_by_key(|r|r.0);
    // An empty/truncated network result must never silently turn funding into zero.
    let expected=(start..=end).step_by(HOUR as usize).collect::<Vec<_>>();
    ensure!(result.iter().map(|r|r.0).collect::<Vec<_>>()==expected,"public funding coverage incomplete");
    Ok(result)
}

pub fn estimate(venue:Venue,market:MarketPair,orders:&std::collections::BTreeMap<String,OrderResult>,time:u64,per_base:Decimal)->Result<Funding> {
    let units=orders.values().flat_map(|o|&o.fills).filter(|f|f.time_ms<time).try_fold(0_i64,|a,f| {
        ensure!(f.venue==venue,"funding journal contains wrong venue");
        a.checked_add(f.units*f.side.sign()).context("funding position overflow")
    })?;
    Ok(Funding {id:format!("paper:{}:{venue:?}:{time}",market.id()),venue,time_ms:time,
        amount:-market.quantity(units)*per_base})
}
