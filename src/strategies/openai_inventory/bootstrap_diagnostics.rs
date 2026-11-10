//! Public bootstrap evidence: typed stages and allowlisted metadata, never raw
//! chained errors, URLs, wallet addresses, signed requests or credentials.
use anyhow::Result;
use serde::Serialize;
use super::MarketPair;

#[derive(Clone, Debug, Serialize)]
pub struct BootstrapIssue {
    pub stage: &'static str,
    pub code: &'static str,
    pub message: String,
}
impl std::fmt::Display for BootstrapIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{} [{} / {}]", self.message, self.stage, self.code) }
}
impl std::error::Error for BootstrapIssue {}

pub fn checked<T>(result: Result<T>, stage: &'static str, message: &'static str) -> Result<T> {
    result.map_err(|error| {
        if error.downcast_ref::<BootstrapIssue>().is_some() { return error; }
        let code = if let Some(http) = error.downcast_ref::<reqwest::Error>() {
            match http.status().map(|s| s.as_u16()) {
                Some(429) => "rate_limited",
                Some(401 | 403) => "http_access_denied",
                Some(_) => "http_error",
                None if http.is_timeout() => "timeout",
                None if http.is_connect() => "connection_failed",
                _ => "response_error",
            }
        } else { "validation_or_unavailable" };
        error.context(BootstrapIssue { stage, code, message: message.into() })
    })
}

pub fn validate_entropy_metadata(market: MarketPair, meta: &crate::hyperliquid::DexAssetMeta) -> Result<()> {
    let failure = if meta.name != market.entropy_symbol() { Some(("symbol_mismatch", "Entropy 合约名称不匹配".into())) }
    else if meta.sz_decimals != 3 { Some(("quantity_precision_changed", format!("Entropy {} 数量精度不匹配：返回 {}，要求 3", market.label(), meta.sz_decimals))) }
    else if meta.is_delisted == Some(true) { Some(("contract_delisted", "Entropy 合约已下架".into())) }
    else if !meta.max_leverage.is_some_and(|v| v >= 3) { Some(("maximum_leverage_unsupported", "Entropy 合约最大杠杆缺失或低于3倍".into())) }
    else if !market.supports_entropy_margin_mode(meta.margin_mode.as_deref()) {
        Some(("margin_mode_unsupported", format!("Entropy {} 返回了无法识别的合约保证金模式", market.label())))
    } else if matches!((meta.margin_mode.as_deref(),meta.only_isolated),
        (Some("noCross" | "strictIsolated"),Some(false)) | (Some("normal"),Some(true))) {
        Some(("isolated_metadata_conflict", "Entropy 逐仓合约信息相互矛盾".into()))
    }
    else { None };
    if let Some((code,message)) = failure { return Err(BootstrapIssue {stage:"entropy_contract",code,message}.into()); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn meta(market: MarketPair, mode: Option<&str>) -> crate::hyperliquid::DexAssetMeta {
        serde_json::from_value(json!({"name":market.entropy_symbol(),"szDecimals":3,"maxLeverage":6,"onlyIsolated":true,"marginMode":mode})).unwrap()
    }
    #[test]
    fn both_documented_isolated_modes_load_for_both_instruments() {
        for market in [MarketPair::Openai,MarketPair::Anth] {
            for mode in ["noCross","strictIsolated"] { validate_entropy_metadata(market,&meta(market,Some(mode))).unwrap(); }
        }
    }
    #[test]
    fn optional_public_restrictions_do_not_imply_the_users_margin_setting() {
        for market in [MarketPair::Openai,MarketPair::Anth] {
            // Synthetic response with the optional restriction fields absent,
            // including deserialization rather than filling a fabricated mode.
            let m:crate::hyperliquid::DexAssetMeta=serde_json::from_value(json!({
                "name":market.entropy_symbol(),"szDecimals":3,"maxLeverage":6
            })).unwrap();
            assert!(m.margin_mode.is_none() && m.only_isolated.is_none());
            validate_entropy_metadata(market,&m).unwrap();
            for only in [None,Some(false),Some(true)] {
                let mut m=m.clone();m.only_isolated=only;
                validate_entropy_metadata(market,&m).unwrap();
            }
            for only in [None,Some(false)] {
                let mut m=m.clone();m.margin_mode=Some("normal".into());m.only_isolated=only;
                validate_entropy_metadata(market,&m).unwrap();
            }
        }
    }
    #[test]
    fn unknown_and_conflicting_modes_still_fail_closed() {
        for market in [MarketPair::Openai,MarketPair::Anth] {
            for mode in [Some(""),Some("cross"),Some("secret-DO-NOT-EXPORT")] {
                let error=validate_entropy_metadata(market,&meta(market,mode)).unwrap_err();
                let issue=error.downcast_ref::<BootstrapIssue>().unwrap();
                assert_eq!(issue.code,"margin_mode_unsupported");
                assert!(!serde_json::to_string(issue).unwrap().contains("secret-DO-NOT-EXPORT"));
            }
            let mut m=meta(market,Some("noCross"));m.only_isolated=Some(false);
            assert_eq!(validate_entropy_metadata(market,&m).unwrap_err().downcast_ref::<BootstrapIssue>().unwrap().code,"isolated_metadata_conflict");
            let m=meta(market,Some("normal"));
            assert_eq!(validate_entropy_metadata(market,&m).unwrap_err().downcast_ref::<BootstrapIssue>().unwrap().code,"isolated_metadata_conflict");
        }
    }
    #[test]
    fn accepting_margin_mode_does_not_relax_symbol_or_quantity_precision() {
        let mut m=meta(MarketPair::Anth,Some("noCross"));m.sz_decimals=4;
        assert_eq!(validate_entropy_metadata(MarketPair::Anth,&m).unwrap_err().downcast_ref::<BootstrapIssue>().unwrap().code,"quantity_precision_changed");
        m.sz_decimals=3;m.name="io:OAI".into();
        assert_eq!(validate_entropy_metadata(MarketPair::Anth,&m).unwrap_err().downcast_ref::<BootstrapIssue>().unwrap().code,"symbol_mismatch");
        m.name=MarketPair::Anth.entropy_symbol().into();m.margin_mode=None;m.only_isolated=None;
        for max in [None,Some(0),Some(2)] {
            m.max_leverage=max;
            assert_eq!(validate_entropy_metadata(MarketPair::Anth,&m).unwrap_err().downcast_ref::<BootstrapIssue>().unwrap().code,"maximum_leverage_unsupported");
        }
        m.max_leverage=Some(6);m.is_delisted=Some(true);
        assert_eq!(validate_entropy_metadata(MarketPair::Anth,&m).unwrap_err().downcast_ref::<BootstrapIssue>().unwrap().code,"contract_delisted");
    }
    #[test]
    fn public_issue_keeps_stage_but_never_echoes_private_source() {
        let raw=anyhow::anyhow!("secret-DO-NOT-EXPORT https://private.invalid/?token=secret");
        let error=checked::<()>(Err(raw),"entropy_api_authorization","Entropy API 授权检查失败").unwrap_err();
        let error=error.context("账户加载失败");
        let issue=error.downcast_ref::<BootstrapIssue>().unwrap();
        let public=serde_json::to_string(issue).unwrap();
        assert!(public.contains("entropy_api_authorization"));
        for private in ["secret", "private.invalid", "token="] { assert!(!public.contains(private)); }
    }
}
