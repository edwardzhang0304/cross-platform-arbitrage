//! Feishu delivery is separate from venue workers and never places orders.
use anyhow::{Context,Result,ensure};
use rusqlite::{OptionalExtension,params};
use serde::{Deserialize,Serialize};
use std::{path::{Path,PathBuf},sync::{Arc,RwLock,atomic::{AtomicBool,Ordering}},time::{Duration,Instant}};
use zeroize::{Zeroize,ZeroizeOnDrop,Zeroizing};
use crate::openai_inventory::alerts;

const QUEUE_ERROR:&str="通知队列暂不可读，将重试；请检查磁盘和文件权限";

#[derive(Clone,Serialize,Deserialize,Zeroize,ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
pub struct FeishuSettings {
    pub app_id:String,
    pub app_secret:String,
    pub receive_id_type:String,
    pub receive_id:String,
}
impl FeishuSettings {
    pub fn validate(&self)->Result<()> {
        ensure!(self.app_id.starts_with("cli_") && self.app_id.len()<=80 && self.app_id.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'_'),"飞书 App ID 格式不正确");
        ensure!((16..=256).contains(&self.app_secret.len()) && !self.app_secret.chars().any(char::is_whitespace),"飞书 App Secret 格式不正确");
        let prefix=match self.receive_id_type.as_str(){"chat_id"=>"oc_","open_id"=>"ou_",_=>anyhow::bail!("请选择群 chat_id 或私聊 open_id")};
        ensure!(self.receive_id.starts_with(prefix) && (10..=128).contains(&self.receive_id.len()) && self.receive_id.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'_'),"飞书接收 ID 格式不正确");
        Ok(())
    }
    pub fn public(&self)->serde_json::Value {
        serde_json::json!({"app_id":self.app_id,"receive_id_type":self.receive_id_type,"receive_id":self.receive_id})
    }
}
#[derive(Clone,Default,Serialize)]
pub struct DeliveryStatus {
    pub enabled:bool,
    pub pending:u64,
    pub dropped:u64,
    pub last_sent_ms:Option<u64>,
    pub error:Option<String>,
}
#[derive(Clone,Default)]
pub struct NotificationHandle {
    settings:Arc<RwLock<Option<FeishuSettings>>>,
    status:Arc<RwLock<DeliveryStatus>>,
    emergency:Arc<RwLock<Option<MemoryAlert>>>,
    persistence_reported:Arc<AtomicBool>,
}
#[derive(Clone)]
struct MemoryAlert {id:String,body:String,attempt:u32,next_ms:u64}
impl NotificationHandle {
    /// A full disk may also prevent writing the outbox. Retain one in-memory
    /// alarm and try delivery independently; no disk write or HTTP on this path.
    pub fn report_persistence_failure(&self,market:crate::openai_inventory::MarketPair,mode:crate::openai_inventory::Mode) {
        if self.persistence_reported.swap(true,Ordering::SeqCst){return;}
        *self.emergency.write().unwrap()=Some(MemoryAlert{id:uuid::Uuid::new_v4().to_string(),
            body:format!("【多平台套利 · 账本写入失败】\n{} · {:?}\n已禁止继续派发交易请求。请立即查看本机控制台、磁盘空间和两平台实际仓位。此通知暂存在内存，程序退出后不能保证补发；不要反复启动或重复下单。",market.id().to_uppercase(),mode),
            attempt:0,next_ms:0});
    }
    pub fn configure(&self,settings:Option<FeishuSettings>) {
        let enabled=settings.is_some();
        *self.settings.write().unwrap()=settings;
        let mut s=self.status.write().unwrap();s.enabled=enabled;s.error=None;
    }
    pub fn status(&self)->DeliveryStatus {self.status.read().unwrap().clone()}
    pub fn spawn(&self,path:PathBuf)->tokio::task::JoinHandle<()> {
        let handle=self.clone();
        tokio::spawn(async move {
            let mut sender=match FeishuSender::new(){Ok(s)=>s,Err(_)=>{handle.status.write().unwrap().error=Some("通知客户端初始化失败".into());return}};
            loop {
                if handle.settings.read().unwrap().is_none(){sender.forget();}
                handle.deliver_emergency(&mut sender).await;
                let result=handle.deliver_once(&path,&mut sender).await;
                if result.is_err(){handle.status.write().unwrap().error=Some(QUEUE_ERROR.into());}
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        })
    }
    async fn deliver_emergency(&self,sender:&mut FeishuSender) {
        let alarm=self.emergency.read().unwrap().clone();
        let cfg=self.settings.read().unwrap().clone();
        let (Some(mut alarm),Some(cfg))=(alarm,cfg) else{return};
        if crate::domain::now_ms()<alarm.next_ms{return;}
        if sender.send(&cfg,&alarm.id,&alarm.body).await.is_ok() {
            *self.emergency.write().unwrap()=None;
            self.status.write().unwrap().last_sent_ms=Some(crate::domain::now_ms());
        } else {
            alarm.next_ms=crate::domain::now_ms().saturating_add(retry_delay_ms(alarm.attempt));
            alarm.attempt=alarm.attempt.saturating_add(1);
            *self.emergency.write().unwrap()=Some(alarm);
        }
    }
    async fn deliver_once(&self,path:&Path,sender:&mut FeishuSender)->Result<()> {
        let now=crate::domain::now_ms();
        // No transaction or database lock survives a network await.
        let (entry,count,dropped)={
            let mut db=alerts::open_delivery(path)?;
            let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            alerts::tick(&tx,now)?;
            tx.commit()?;
            let count:u64=db.query_row("SELECT COUNT(*) FROM notification_outbox",[],|r|r.get(0))?;
            let dropped:u64=db.query_row("SELECT dropped FROM notification_counters WHERE id=1",[],|r|r.get(0))?;
            let entry:Option<(String,String,u32)>=db.query_row("SELECT id,body,attempts FROM notification_outbox WHERE id=(SELECT id FROM notification_outbox ORDER BY at_ms,rowid LIMIT 1) AND next_ms<=?1",[now],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            (entry,count,dropped)
        };
        {let mut status=self.status.write().unwrap();status.pending=count;status.dropped=dropped;
            // A successful queue read is enough to clear a prior database
            // error, including an empty queue. Keep a real send failure while
            // its durable message is still waiting for its retry deadline.
            if count==0 || status.error.as_deref()==Some(QUEUE_ERROR) {status.error=None;}
        }
        let Some(settings)=self.settings.read().unwrap().clone() else {sender.forget();return Ok(())};
        let Some((id,body,attempts))=entry else{return Ok(())};
        let body=if dropped>0 {format!("{body}\n通知队列曾超限，已丢弃最旧 {dropped} 条通知；详情请检查本机账本。")}else{body};
        // Token acquisition may be slow. Recheck cancellation immediately before
        // the message POST, with no SQL lock held over the network request.
        let result=sender.send_checked(&settings,&id,&body,||alerts::queued(path,&id)).await;
        let mut db=alerts::open_delivery(path)?;
        match result {
            Ok(true)=>{
                let tx=db.transaction()?;
                tx.execute("DELETE FROM notification_outbox WHERE id=?1",[&id])?;
                alerts::delivered(&tx,&id,crate::domain::now_ms())?;
                tx.commit()?;
                let mut status=self.status.write().unwrap();status.last_sent_ms=Some(crate::domain::now_ms());status.error=None;status.pending=count.saturating_sub(1);
            }
            Ok(false)=>{let mut status=self.status.write().unwrap();status.error=None;status.pending=count.saturating_sub(1);}
            Err(_)=>{
                let next=crate::domain::now_ms().saturating_add(retry_delay_ms(attempts));
                let changed=db.execute("UPDATE notification_outbox SET attempts=attempts+1,next_ms=?1 WHERE id=?2",params![next,id])?;
                self.status.write().unwrap().error=if changed>0 {Some("飞书发送失败，将自动重试；检查网络、机器人权限和接收 ID".into())}else{None};
            }
        }
        Ok(())
    }
}
fn retry_delay_ms(attempt:u32)->u64 {match attempt {0=>5_000,1=>15_000,2=>60_000,_=>300_000}}

struct FeishuSender {
    client:reqwest::Client,
    // Production endpoints cannot be overridden by configuration or API input.
    base:String,
    token:Option<(Zeroizing<String>,Instant)>,
    identity:Option<FeishuSettings>,
}
#[derive(Deserialize,Zeroize,ZeroizeOnDrop)]
struct TokenResponse {code:i64,#[serde(default)] tenant_access_token:String,#[serde(default)] expire:u64}
#[derive(Deserialize)]
struct MessageResponse {code:i64}
#[derive(Serialize)]
struct TokenRequest<'a> {app_id:&'a str,app_secret:&'a str}
async fn response_body(mut response:reqwest::Response)->Result<Zeroizing<Vec<u8>>> {
    let mut body=Zeroizing::new(Vec::new());
    while let Some(chunk)=response.chunk().await.context("飞书响应读取失败")? {
        ensure!(body.len().saturating_add(chunk.len())<=64*1024,"飞书响应超出限制");
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
impl FeishuSender {
    fn new()->Result<Self> {
        Ok(Self{client:reqwest::Client::builder().timeout(Duration::from_secs(8))
            .connect_timeout(Duration::from_secs(3)).redirect(reqwest::redirect::Policy::none())
            .build()?,base:"https://open.feishu.cn/open-apis".into(),token:None,identity:None})
    }
    fn forget(&mut self){self.token=None;self.identity=None;}
    async fn send(&mut self,cfg:&FeishuSettings,id:&str,body:&str)->Result<()> {
        self.send_checked(cfg,id,body,||Ok(true)).await.map(|_|())
    }
    async fn send_checked(&mut self,cfg:&FeishuSettings,id:&str,body:&str,still_needed:impl FnOnce()->Result<bool>)->Result<bool> {
        cfg.validate()?;
        // Do not accidentally reuse a previous application's token after edits.
        if self.identity.as_ref().is_none_or(|x|x.app_id!=cfg.app_id||x.app_secret!=cfg.app_secret) {
            self.forget();self.identity=Some(cfg.clone());
        }
        if self.token.as_ref().is_none_or(|(_,expiry)|Instant::now()>=*expiry) {
            let request=Zeroizing::new(serde_json::to_string(&TokenRequest{app_id:&cfg.app_id,app_secret:&cfg.app_secret})?);
            let response=self.client.post(format!("{}/auth/v3/tenant_access_token/internal",self.base))
                .header("Content-Type","application/json; charset=utf-8").body(request.as_bytes().to_vec())
                .send().await.context("飞书令牌请求失败")?;
            ensure!(response.status().is_success(),"飞书令牌请求失败");
            let bytes=response_body(response).await?;
            let mut data:TokenResponse=serde_json::from_slice(&bytes).context("飞书令牌响应无效")?;
            ensure!(data.code==0 && !data.tenant_access_token.is_empty() && data.expire>=60,"飞书授权失败");
            let ttl=data.expire.saturating_sub(60).min(7200);
            self.token=Some((Zeroizing::new(std::mem::take(&mut data.tenant_access_token)),Instant::now()+Duration::from_secs(ttl)));
        }
        if !still_needed()? {return Ok(false);}
        let token=&self.token.as_ref().unwrap().0;
        let response=self.client.post(format!("{}/im/v1/messages",self.base))
            .query(&[("receive_id_type",&cfg.receive_id_type)])
            .bearer_auth(token.as_str()).json(&serde_json::json!({
                "receive_id":cfg.receive_id,"msg_type":"text",
                "content":serde_json::json!({"text":body}).to_string(),"uuid":id,
            })).send().await.context("飞书消息请求失败")?;
        if !response.status().is_success(){self.token=None;anyhow::bail!("飞书消息请求失败");}
        let data:MessageResponse=serde_json::from_slice(&response_body(response).await?).context("飞书消息响应无效")?;
        if data.code!=0{self.token=None;anyhow::bail!("飞书消息未被接受");}
        Ok(true)
    }
}
pub async fn test_message(settings:&FeishuSettings,market:crate::openai_inventory::MarketPair)->Result<()> {
    FeishuSender::new()?.send(settings,&uuid::Uuid::new_v4().to_string(),
        &format!("【多平台套利 · 测试通知】\n{} 实盘异常通知测试。此消息不会触发任何交易。",market.id().to_uppercase())).await
        .map_err(|_|anyhow::anyhow!("飞书测试失败；检查网络、机器人是否已发布、接收人可用范围、发送权限及接收 ID"))
}

#[cfg(test)]
#[path="notification_tests.rs"]
mod tests;
