//! Bounded entry completion, separate from unbounded reducing recovery.
use super::*;

pub(super) const DELAY_MS:u64=3_000;
const MAX_ATTEMPTS:u32=4;

fn eligible(s:&Snapshot)->bool {
    s.pending.as_ref().is_some_and(|p|p.action==Action::Open && p.first_venue==Venue::Entropy
        && p.first_terminal && p.first_filled>p.hedge_filled && p.hedge_terminal
        && p.repair.is_none() && p.repair_filled==0 && !p.repair_terminal
        && p.unwind_hedge.is_none() && p.unwind_hedge_filled==0 && p.align_close.is_none()
        && p.entry_hedge_retry.as_ref().is_some_and(|r|!r.abandoned))
}
fn stopped(s:&Snapshot)->bool {
    s.paused || s.stop_requested || s.close_requested || s.stop_after_close
        || s.loss_stop.is_some() || s.live_orphan.is_some() || s.liquidation_protection.is_some()
        || super::super::emergency_exit::active(s)
}
pub fn due(s:&Snapshot,now:u64)->bool {
    s.status!=Status::NeedsAttention && eligible(s) && !stopped(s)
        && s.pending.as_ref().unwrap().entry_hedge_retry.as_ref().unwrap()
            .after_ms.is_some_and(|t|now>=t)
        && s.pending.as_ref().unwrap().entry_hedge_retry.as_ref().unwrap().attempt<MAX_ATTEMPTS
}

fn save(s:&mut Snapshot,store:&mut Store,next:Snapshot,now:u64,event:&str)->Result<()> {
    store.commit(&next,now,event)?;*s=next;Ok(())
}

/// A failed account read never authorizes a new order or advances the ladder.
pub fn defer_account_read(s:&mut Snapshot,store:&mut Store,now:u64)->Result<()> {
    let mut next=s.clone();
    if let Some(r)=next.pending.as_mut().and_then(|p|p.entry_hedge_retry.as_mut()) {
        r.after_ms=Some(now.saturating_add(DELAY_MS));
        r.reason="fresh account verification unavailable; no hedge retry sent".into();
        next.reason=r.reason.clone();
        save(s,store,next,now,"entry_hedge_account_wait")?;
    }
    Ok(())
}

/// true means wait; false lets the ordinary workflow dispatch/repair/finish.
/// Retry requests are never persisted here: advance persists before submitting,
/// and any restored attempt marker follows the existing lookup-only path.
pub(super) fn prepare(s:&mut Snapshot,store:&mut Store,books:&[Book;2],now:u64,
    accounts:Option<&[AccountEvidence;2]>)->Result<bool> {
    if !eligible(s) {return Ok(false);}
    let mut next=s.clone();
    let exhausted=s.pending.as_ref().unwrap().entry_hedge_retry.as_ref().unwrap().attempt>=MAX_ATTEMPTS;
    if stopped(s) || exhausted {
        let r=next.pending.as_mut().unwrap().entry_hedge_retry.as_mut().unwrap();
        r.abandoned=true;r.after_ms=None;
        r.reason=if exhausted {"entry hedge retry ladder exhausted; reducing unpaired exposure"}
            else {"entry hedge retries cancelled by stop or risk protection"}.into();
        save(s,store,next,now,"entry_hedge_retry_cancelled")?;
        return Ok(false);
    }
    let retry=next.pending.as_mut().unwrap().entry_hedge_retry.as_mut().unwrap();
    if retry.after_ms.is_none() {
        retry.after_ms=Some(now.saturating_add(DELAY_MS));
        retry.reason="terminal hedge incomplete; waiting three seconds before verified retry".into();
        next.status=Status::Recovering;next.reason=retry.reason.clone();
        save(s,store,next,now,"entry_hedge_retry_scheduled")?;
        return Ok(true);
    }
    if now<retry.after_ms.unwrap() {return Ok(true);}
    let Some(a)=accounts else {return Ok(true);};
    // Fresh REST evidence for BOTH legs, including ownership of all earlier
    // partial fills. A position delta is never manufactured into a fill.
    s.assert_reconciled(a)?;
    for (i,account) in a.iter().enumerate() {
        ensure!(account.venue.index()==i && account.authenticated && account.observed_ms>0
            && account.observed_ms<=now && now-account.observed_ms<=s.config.account_max_age_ms,
            "entry hedge retry requires fresh authenticated accounts");
        ensure!(account.account==if i==0 {s.config.lighter_account.clone()}else{s.config.entropy_account.clone()},
            "entry hedge retry account binding mismatch");
        ensure!(account.open_orders==0,"entry hedge retry has unresolved venue orders");
    }
    let op=s.pending.as_ref().unwrap();
    ensure!(op.first_filled>0 && op.first_filled<=op.requested_units && op.hedge_filled>=0
        && op.hedge_filled<op.first_filled && op.first_filled%s.config.common_step()==0,
        "invalid entry hedge retry quantities");
    for venue in [Venue::Lighter,Venue::Entropy] {
        let owned=s.paired_units()+if venue==op.first_venue {op.first_filled}else{op.hedge_filled};
        ensure!(s.positions[venue.index()].units==s.direction.open_side(venue).sign()*owned,
            "entry hedge retry ownership differs from paired inventory");
    }
    // A slow reconciliation must not reuse the pre-await book. Caller supplies
    // current books; unavailable books get a bounded grace before reducing.
    if books.iter().any(|b|b.validate(now,s.config.book_max_age_ms).is_err()) {
        let p=next.pending.as_mut().unwrap();
        let start=*p.quote_wait_started_ms.get_or_insert(now);
        let r=p.entry_hedge_retry.as_mut().unwrap();
        r.after_ms=Some(now.saturating_add(DELAY_MS));
        r.abandoned=now.saturating_sub(start)>=s.config.operation_timeout_ms;
        r.reason="entry hedge retry waiting for fresh books".into();next.reason=r.reason.clone();
        let waiting=!r.abandoned;
        save(s,store,next,now,"entry_hedge_quote_wait")?;
        return Ok(waiting);
    }
    let p=next.pending.as_mut().unwrap();
    p.quote_wait_started_ms=None;
    let r=p.entry_hedge_retry.as_mut().unwrap();
    r.attempt+=1;r.after_ms=Some(now.saturating_add(DELAY_MS));r.reason.clear();
    let qty=op.first_filled-op.hedge_filled;
    let side=s.direction.open_side(Venue::Lighter);
    let candidate=request(&next,books,Venue::Lighter,side,qty,false,"hedge",now);
    let gate=candidate.as_ref().map_err(|e|e.to_string()).and_then(|request| {
        let account=&a[0];
        let notional=s.config.quantity(qty)*request.limit;
        if qty<s.config.market.minimum_units(Venue::Lighter) {return Err("hedge below venue minimum base quantity".into());}
        if a.iter().any(|x|!x.isolated || x.leverage!=s.config.leverage) {return Err("hedge retry leverage/margin mode mismatch".into());}
        if s.lots.len()>=s.config.max_groups {return Err("hedge retry group limit".into());}
        if s.config.quantity(account.position_units.abs()+qty)*request.limit>s.config.max_notional_per_venue {return Err("hedge retry inventory notional cap".into());}
        if account.free_margin-notional/Decimal::from(s.config.leverage)-notional*s.config.fee_lighter<s.config.min_free_margin {return Err("hedge retry insufficient free margin".into());}
        Ok(())
    });
    if let Err(reason)=gate {
        let retry=next.pending.as_mut().unwrap().entry_hedge_retry.as_mut().unwrap();
        // Depth/price failures consume a bounded ladder step. A non-price risk
        // failure abandons entry completion and preserves reducing recovery.
        let price=matches!(reason.as_str(),"depth exceeds protected execution price"|"insufficient depth"|"hedge would breach paired entry spread budget");
        retry.abandoned=!price || retry.attempt>=MAX_ATTEMPTS;
        retry.reason=reason.clone();next.reason=format!("entry hedge retry unavailable: {reason}");
        let waiting=!retry.abandoned;
        save(s,store,next,now,"entry_hedge_retry_not_sent")?;
        return Ok(waiting);
    }
    let p=next.pending.as_mut().unwrap();
    p.hedge=None;p.hedge_terminal=false;
    p.entry_hedge_retry.as_mut().unwrap().after_ms=None;
    next.status=Status::Recovering;
    next.reason="terminal hedge and owned positions verified; retrying remaining entry quantity".into();
    // advance immediately builds the same request and writes its attempt marker.
    // If persistence fails below, no submit occurs and restart retains the prior
    // terminal order, without losing fills or resending an unknown request.
    *s=next;
    Ok(false)
}

#[cfg(test)]
#[path = "entry_hedge_retry_tests.rs"]
mod tests;
