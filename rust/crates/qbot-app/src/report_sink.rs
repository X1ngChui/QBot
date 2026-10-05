//! Delivering reports to owners as private messages over the platform connection.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qbot_core::AccountId;
use qbot_gateway::bridge::Bridge;
use qbot_ops::{OpsError, ReportSink};
use serde_json::json;

#[derive(Debug)]
pub struct OneBotReportSink {
    bridge: Arc<Bridge>,
    timeout: Duration,
}

impl OneBotReportSink {
    pub fn new(bridge: Arc<Bridge>, timeout: Duration) -> Self {
        Self { bridge, timeout }
    }
}

#[async_trait]
impl ReportSink for OneBotReportSink {
    async fn send(&self, to: AccountId, text: &str) -> Result<(), OpsError> {
        let params = json!({ "user_id": to.get(), "message": [{ "type": "text", "data": { "text": text } }] });
        let response = self
            .bridge
            .call("send_private_msg", params, self.timeout)
            .await
            .map_err(|e| OpsError(e.to_string()))?;
        if response.ok {
            Ok(())
        } else {
            Err(OpsError(format!(
                "the platform refused the message: {}",
                if response.detail.is_empty() {
                    format!("retcode {}", response.retcode)
                } else {
                    response.detail
                }
            )))
        }
    }
}

/// NapCat's own cache cleanup (`clean_cache`), asked for over the platform connection. NapCat
/// decides what its cache is; the bot never reads or deletes NapCat's files itself.
#[derive(Debug)]
pub struct NapCatCache {
    bridge: Arc<Bridge>,
    timeout: Duration,
}

impl NapCatCache {
    pub fn new(bridge: Arc<Bridge>, timeout: Duration) -> Self {
        Self { bridge, timeout }
    }
}

#[async_trait]
impl qbot_ops::PlatformCache for NapCatCache {
    async fn clean(&self) -> Result<(), OpsError> {
        let response = self
            .bridge
            .call("clean_cache", json!({}), self.timeout)
            .await
            .map_err(|e| OpsError(e.to_string()))?;
        if response.ok {
            Ok(())
        } else {
            Err(OpsError(format!(
                "the platform refused to clean its cache: retcode {} {}",
                response.retcode, response.detail
            )))
        }
    }
}
