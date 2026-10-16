//! Host identities are independent of instruments. No credentials or trading rules live here.
use crate::openai_inventory::{InventoryConfig,MarketPair,Mode};
use anyhow::{Result,ensure};
use serde::{Deserialize,Serialize};
use std::{collections::BTreeSet,path::Path};

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountProfile { pub id:String, pub label:String, pub market:MarketPair,
    #[serde(default="crate::openai_inventory::config::default_capital_multiplier")]
    pub capital_multiplier:u32 }
impl AccountProfile {
    pub fn legacy(market:MarketPair)->Self {Self {id:market.id().into(),label:format!("{} 账号1",market.label()),market,capital_multiplier:1}}
    pub fn new(market:MarketPair,label:String,request_id:&str)->Result<Self> {
        let uuid=uuid::Uuid::parse_str(request_id)?;
        let p=Self{id:format!("p-{}",uuid.simple()),label,market,capital_multiplier:1};p.validate()?;Ok(p)
    }
    pub fn is_legacy(&self)->bool {self.id==self.market.id()}
    pub fn validate(&self)->Result<()> {
        ensure!([1,10].contains(&self.capital_multiplier) && (!self.is_legacy() || self.capital_multiplier==1),"invalid account capital tier");
        let generated=self.id.strip_prefix("p-").is_some_and(|s|s.len()==32 && s.bytes().all(|c|c.is_ascii_digit()||(b'a'..=b'f').contains(&c)));
        ensure!(self.is_legacy()||generated,"invalid immutable account profile ID");
        ensure!(!self.label.trim().is_empty() && self.label==self.label.trim() && self.label.chars().count()<=40
            && !self.label.chars().any(char::is_control),"账户名称须为 1～40 个字符，不能含控制字符或首尾空格");
        Ok(())
    }
    pub fn validate_config(&self,c:&InventoryConfig)->Result<()> {
        self.validate()?;c.validate()?;c.validate_live_identity()?;
        ensure!(c.market==self.market && c.mode==Mode::Live,"account profile market/mode mismatch");
        ensure!(c.capital_multiplier==self.capital_multiplier,"account capital tier differs from its immutable profile");
        if !self.is_legacy() {
            let mut expected=c.clone();expected.set_initial_capital(self.capital_multiplier)?;
            ensure!(expected==*c,"new account capital settings differ from its approved tier");
        }
        if !self.is_legacy() || self.market==MarketPair::Anth {
            ensure!(c.lighter_account==self.alias("lighter") && c.entropy_account==self.alias("entropy"),"account profile aliases do not match its ID");
        }
        if self.market==MarketPair::Anth {
            ensure!(c.lighter_address.as_ref().is_some_and(|a|!a.eq_ignore_ascii_case(&c.entropy_address)),"ANTH requires separate wallets for the two platforms");
        }
        Ok(())
    }
    pub fn alias(&self,venue:&str)->String {format!("{}:live:{venue}",self.id)}
    pub fn control_path(&self)->String {if self.is_legacy(){if self.market==MarketPair::Openai{"/".into()}else{"/anth".into()}}else{format!("/accounts/{}",self.id)}}
    pub fn control_api(&self)->String {if self.is_legacy(){if self.market==MarketPair::Openai{"/api/portable".into()}else{"/api/anth-portable".into()}}else{format!("/api/accounts/{}/control",self.id)}}
    pub fn inventory_api(&self)->String {if self.is_legacy(){format!("/api/{}-inventory",self.id)}else{format!("/api/accounts/{}/inventory",self.id)}}
    pub fn monitor_path(&self)->String {if self.is_legacy(){format!("/{}-inventory",self.id)}else{format!("/accounts/{}/monitor",self.id)}}
    pub fn start_confirmation(&self)->String {format!("START_{}_LIVE_STRATEGY",self.id.to_uppercase())}
    pub fn recovery_confirmation(&self)->String {format!("CONFIRM_{}_ROLLBACK_RECOVERY",self.id.to_uppercase())}
    pub fn descriptor(&self)->serde_json::Value {serde_json::json!({"id":self.id,"label":self.label,"market":self.market,"mode":"live","capital_multiplier":self.capital_multiplier,
        "control_path":self.control_path(),"control_api":self.control_api(),"inventory_api":self.inventory_api(),"monitor_path":self.monitor_path()})}
    pub fn notification_label(&self)->String {format!("{} · {} · {}",self.label,self.market.label(),self.id)}
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {schema:u32,profiles:Vec<AccountProfile>}
pub fn load(root:&Path)->Result<Vec<AccountProfile>> {
    let file=root.join("profiles/accounts.json");
    let profiles=if file.exists(){ensure!(std::fs::metadata(&file)?.len()<=131_072,"profile catalog exceeds size limit");let c:Catalog=serde_json::from_slice(&std::fs::read(file)?)?;ensure!(c.schema==1,"unknown profile catalog schema");c.profiles}
        else{vec![AccountProfile::legacy(MarketPair::Openai),AccountProfile::legacy(MarketPair::Anth)]};
    validate_catalog(&profiles)?;Ok(profiles)
}
pub fn save(root:&Path,profiles:&[AccountProfile])->Result<()> {
    validate_catalog(profiles)?;
    crate::portable::atomic_json(&root.join("profiles/accounts.json"),&Catalog{schema:1,profiles:profiles.to_vec()})
}
fn validate_catalog(profiles:&[AccountProfile])->Result<()> {
    ensure!((2..=32).contains(&profiles.len()),"最多支持 32 套独立账户");
    let mut ids=BTreeSet::new();let mut labels=BTreeSet::new();
    for p in profiles {p.validate()?;ensure!(ids.insert(&p.id)&&labels.insert(&p.label),"账户编号或名称重复");}
    for market in [MarketPair::Openai,MarketPair::Anth] {ensure!(profiles.iter().any(|p|p.is_legacy()&&p.market==market),"original account profile missing");}
    Ok(())
}
pub fn html_escape(s:&str)->String {s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;").replace('"',"&quot;").replace('\'',"&#39;")}
pub fn script_json(value:&serde_json::Value)->String {value.to_string().replace('<',"\\u003c").replace('>',"\\u003e").replace('&',"\\u0026").replace('\u{2028}',"\\u2028").replace('\u{2029}',"\\u2029")}
pub fn navigation(selected:&AccountProfile,profiles:&[AccountProfile],monitor:bool)->String {
    let mut links="<a href=\"/accounts\">账户管理</a>".to_string();
    if monitor {links+=&format!("<a href=\"{}\">当前账户配置</a>",selected.control_path());}
    for p in profiles {links+=&format!("<a href=\"{}\"{}>{}</a>",if monitor{p.monitor_path()}else{p.control_path()},
        if p.id==selected.id{" aria-current=\"page\" style=\"font-weight:700\""}else{""},html_escape(&p.label));}
    format!("<nav aria-label=\"账户切换\" style=\"display:flex;flex-wrap:wrap;gap:12px 22px;margin-bottom:18px\">{links}</nav>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::ProfilePaths;
    #[test]
    fn catalog_keeps_legacy_paths_and_roundtrips_multiple_same_market_profiles() {
        let root=std::env::temp_dir().join(format!("cpa-profiles-{}",uuid::Uuid::new_v4()));
        let mut profiles=load(&root).unwrap();assert!(!root.exists());
        let original=profiles.clone();
        let extra=AccountProfile::new(MarketPair::Openai,"OPENAI 账号2".into(),&uuid::Uuid::new_v4().to_string()).unwrap();
        profiles.push(extra.clone());save(&root,&profiles).unwrap();assert_eq!(load(&root).unwrap(),profiles);
        assert_eq!(ProfilePaths::for_profile(&root,original[0].clone()).database,root.join("runtime/openai-inventory/live.sqlite"));
        assert_eq!(ProfilePaths::for_profile(&root,original[1].clone()).database,root.join("profiles/anth-live/runtime/inventory.sqlite"));
        let paths=profiles.iter().map(|p|ProfilePaths::for_profile(&root,p.clone())).collect::<Vec<_>>();
        for i in 0..paths.len(){for j in 0..i{assert_ne!(paths[i].database,paths[j].database);assert_ne!(paths[i].vault,paths[j].vault);assert_ne!(profiles[i].control_api(),profiles[j].control_api());}}
        let previous=std::fs::read(root.join("profiles/accounts.json")).unwrap();
        profiles.push(extra);assert!(save(&root,&profiles).is_err());
        assert_eq!(std::fs::read(root.join("profiles/accounts.json")).unwrap(),previous);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn catalog_rejects_identity_traversal_aliases_duplicate_labels_and_missing_legacy() {
        let legacy=vec![AccountProfile::legacy(MarketPair::Openai),AccountProfile::legacy(MarketPair::Anth)];
        for id in ["../openai","OPENAI","anth","p-../foo","p-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"] {
            assert!(AccountProfile{id:id.into(),label:"x".into(),market:MarketPair::Openai,capital_multiplier:1}.validate().is_err());
        }
        for label in ["","  "," ends ","new\naccount"] {
            assert!(AccountProfile::new(MarketPair::Openai,label.into(),&uuid::Uuid::new_v4().to_string()).is_err());
        }
        let mut profiles=legacy.clone();profiles.push(AccountProfile::new(MarketPair::Anth,legacy[0].label.clone(),&uuid::Uuid::new_v4().to_string()).unwrap());
        assert!(validate_catalog(&profiles).is_err());assert!(validate_catalog(&legacy[1..]).is_err());
    }
    #[test]
    fn label_cannot_break_html_or_script_and_confirmation_is_id_scoped() {
        let p=AccountProfile::new(MarketPair::Openai,"</script><img src=x>".into(),&uuid::Uuid::new_v4().to_string()).unwrap();
        let nav=navigation(&p,&[p.clone()],false);assert!(!nav.contains("<img"));assert!(nav.contains("&lt;img"));
        let js=script_json(&p.descriptor());assert!(!js.contains("</script>"));assert_eq!(serde_json::from_str::<serde_json::Value>(&js).unwrap(),p.descriptor());
        assert_ne!(p.start_confirmation(),AccountProfile::legacy(MarketPair::Openai).start_confirmation());
        assert_eq!(AccountProfile::new(p.market,p.label.clone(),&p.id[2..]).unwrap(),p);
    }
}
