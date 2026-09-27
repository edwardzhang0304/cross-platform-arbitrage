use anyhow::Result;
use serde_json::Value;
use crate::lighter::LighterSubmitRejected;
#[derive(Debug)]
pub(crate) enum LighterSubmitDisposition {
    Accepted(Value),
    Rejected(anyhow::Error),
    Ambiguous(anyhow::Error),
}

pub(crate) fn classify_submit_result(result: Result<Value>) -> LighterSubmitDisposition {
    match result {
        Ok(response) if response_is_accepted(&response) => {
            LighterSubmitDisposition::Accepted(response)
        }
        Ok(response) => LighterSubmitDisposition::Rejected(anyhow::anyhow!(
            "Lighter sendTx returned an explicit rejection: {response}"
        )),
        Err(error) if error.downcast_ref::<LighterSubmitRejected>().is_some() => {
            LighterSubmitDisposition::Rejected(error)
        }
        Err(error) => LighterSubmitDisposition::Ambiguous(error),
    }
}

fn response_is_accepted(v: &Value)->bool { v.get("code").and_then(Value::as_i64)==Some(200) }
