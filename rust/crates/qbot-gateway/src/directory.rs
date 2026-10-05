//! Members' current group display names, asked of the platform when needed.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::Directory;
use qbot_core::{AccountId, GroupId};
use serde_json::{Value, json};

use crate::bridge::Bridge;

#[derive(Debug)]
pub struct OneBotDirectory {
    bridge: Arc<Bridge>,
    timeout: Duration,
}

impl OneBotDirectory {
    pub fn new(bridge: Arc<Bridge>, timeout: Duration) -> Self {
        Self { bridge, timeout }
    }
}

fn clean(value: Option<&Value>) -> Option<String> {
    let text: String = value?
        .as_str()?
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[async_trait]
impl Directory for OneBotDirectory {
    /// `get_group_member_info`: the group nickname (`card`), else the account nickname
    /// (`nickname`), which is what QQ shows in the group. Asked on
    /// every use and kept nowhere in the bot. NapCat answers from the member list QQ keeps current
    /// through its own change notifications; forcing a fetch from QQ (`no_cache`) for every name
    /// would cost a round trip per member for lists like `/members`.
    async fn display_name(&self, group: GroupId, account: AccountId) -> Option<String> {
        let params =
            json!({ "group_id": group.get(), "user_id": account.get(), "no_cache": false });
        let response = self
            .bridge
            .call("get_group_member_info", params, self.timeout)
            .await
            .ok()?;
        if !response.ok {
            return None;
        }
        clean(response.data.get("card")).or_else(|| clean(response.data.get("nickname")))
    }
}
