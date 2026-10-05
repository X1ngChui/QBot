//! Sending to a group over the platform connection and waiting for the echo.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::{Delivered, Delivery, DeliveryError, OutSegment};
use qbot_core::GroupId;
use serde_json::{Value, json};

use crate::bridge::{Bridge, CallError};
use crate::echo::EchoBoard;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryTimeouts {
    /// How long the platform may take to accept a send.
    pub action: Duration,
    /// How long after acceptance the bot's own message may take to be reported back.
    pub echo: Duration,
}

#[derive(Debug)]
pub struct OneBotDelivery {
    bridge: Arc<Bridge>,
    echoes: Arc<EchoBoard>,
    timeouts: DeliveryTimeouts,
}

impl OneBotDelivery {
    pub fn new(bridge: Arc<Bridge>, echoes: Arc<EchoBoard>, timeouts: DeliveryTimeouts) -> Self {
        Self {
            bridge,
            echoes,
            timeouts,
        }
    }
}

fn wire(segment: &OutSegment) -> Value {
    match segment {
        OutSegment::Text(text) => json!({"type": "text", "data": {"text": text}}),
        OutSegment::At(account) => json!({"type": "at", "data": {"qq": account.get().to_string()}}),
        OutSegment::Reply(message) => {
            json!({"type": "reply", "data": {"id": message.get().to_string()}})
        }
        OutSegment::Face(id) => json!({"type": "face", "data": {"id": id.to_string()}}),
        OutSegment::Dice => json!({"type": "dice", "data": {}}),
        OutSegment::Rps => json!({"type": "rps", "data": {}}),
        OutSegment::Contact(account) => {
            json!({"type": "contact", "data": {"type": "qq", "id": account.get().to_string()}})
        }
    }
}

#[async_trait]
impl Delivery for OneBotDelivery {
    async fn send(
        &self,
        group: GroupId,
        segments: Vec<OutSegment>,
    ) -> Result<Delivered, DeliveryError> {
        let params = json!({
            "group_id": group.get(),
            "message": segments.iter().map(wire).collect::<Vec<_>>(),
        });
        let response = self
            .bridge
            .call("send_group_msg", params, self.timeouts.action)
            .await
            .map_err(|e| match e {
                CallError::Disconnected => {
                    DeliveryError::Unavailable("not connected to the platform".into())
                }
                CallError::TimedOut(after) => DeliveryError::Unavailable(format!(
                    "no answer within {after:?}; the message may or may not have been sent"
                )),
            })?;
        if !response.ok {
            let detail = if response.detail.is_empty() {
                format!("retcode {}", response.retcode)
            } else {
                response.detail
            };
            return Err(DeliveryError::Rejected(detail));
        }
        let Some(message) = response.message_id else {
            return Err(DeliveryError::Rejected(
                "the platform accepted the send but returned no message id".into(),
            ));
        };
        match self.echoes.wait(group, message, self.timeouts.echo).await {
            Some(echo) => Ok(Delivered { echo }),
            None => Err(DeliveryError::EchoMissing),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use qbot_core::AccountId;

    use super::*;

    #[test]
    fn a_contact_card_is_napcats_contact_segment_for_a_qq_account() {
        assert_eq!(
            wire(&OutSegment::Contact(AccountId::new(12345).unwrap())),
            json!({"type": "contact", "data": {"type": "qq", "id": "12345"}})
        );
    }
}
