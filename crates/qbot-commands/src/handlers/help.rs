//! `/help`: the commands, or one command in detail.

use qbot_i18n::Msg;

use super::Reply;
use crate::catalog::Command;
use crate::router::Cx;

pub(crate) async fn help(cx: &Cx<'_>) -> Reply {
    if !cx.req.mentions.is_empty() {
        return Err(Msg::UsageHelp {}.into());
    }
    let tokens = cx.tokens();
    match tokens.as_slice() {
        [] => {
            let mut lines = vec![cx.t(&Msg::HelpHeader {})];
            let mut last = None;
            for command in Command::ALL {
                if last != Some(command.category()) {
                    lines.push(format!("\n[{}]", cx.t(&command.category().title())));
                    last = Some(command.category());
                }
                let mut line = cx.t(&Msg::HelpEntry {
                    command: format!("/{}", command.name()),
                    what: cx.t(&command.what()),
                });
                if command.owner_only() {
                    line.push(' ');
                    line.push_str(&cx.t(&if command.delegated() {
                        Msg::HelpDelegatedTag {}
                    } else {
                        Msg::HelpOwnerTag {}
                    }));
                }
                lines.push(line);
            }
            lines.push(format!("\n{}", cx.t(&Msg::HelpFooter {})));
            Ok(lines.join("\n"))
        }
        [name] => {
            let command = Command::find(name).ok_or_else(|| Msg::HelpUnknown {
                name: (*name).to_owned(),
            })?;
            let access = cx.t(&if command.delegated() {
                Msg::HelpAccessDelegated {}
            } else if command.owner_only() {
                Msg::HelpAccessOwner {}
            } else {
                Msg::HelpAccessMember {}
            });
            Ok(cx.t(&Msg::HelpDetail {
                command: format!("/{}", command.name()),
                what: cx.t(&command.what()),
                access,
                detail: cx.t(&command.detail()),
            }))
        }
        _ => Err(Msg::UsageHelp {}.into()),
    }
}
