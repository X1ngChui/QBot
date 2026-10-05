use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jiff::tz::TimeZone;
use qbot_agent::Directory;
use qbot_agent::{Delivery, OutSegment};
use qbot_core::{AccountId, Clock, GroupId};
use qbot_gateway::pipeline::{CommandRequest, Commands};
use qbot_i18n::{Locales, Msg};
use qbot_memory::IdentityStore;
use qbot_memory::NoteStore;
use qbot_memory::facts::{DecayPolicy, FactStore};
use qbot_sched::TaskService;

use crate::catalog::Command;
use crate::fail::Fail;
use crate::ports::{GroupAdmin, RecentLogs, RunHistory};
use crate::{handlers, reply};

#[derive(Debug, Clone)]
pub struct CommandSettings {
    pub bot: AccountId,
    pub owners: BTreeSet<AccountId>,
    pub zone: TimeZone,
    /// The platform's cap on one message, in characters. Replies are split to fit.
    pub max_message_chars: usize,
    /// Rows `/members` shows.
    pub members_max_rows: usize,
    /// Most rows `/top` shows.
    pub top_max_rows: usize,
    /// How long a link invitation stays open (shown in the reply that issues it).
    pub link_ttl: Duration,
    /// How confident a learned fact still is, for /who.
    pub fact_decay: DecayPolicy,
    /// Most notes one account may have in a group: notes reach the model through member lookups.
    pub notes_per_account: usize,
    /// Rows `/runs` lists.
    pub runs_max_rows: usize,
}

pub struct Deps {
    pub identity: Arc<dyn IdentityStore>,
    pub facts: Arc<dyn FactStore>,
    pub notes: Arc<dyn NoteStore>,
    pub admin: Arc<dyn GroupAdmin>,
    pub runs: Arc<dyn RunHistory>,
    pub logs: Arc<dyn RecentLogs>,
    pub tasks: TaskService,
    pub directory: Arc<dyn Directory>,
    pub delivery: Arc<dyn Delivery>,
    pub locales: Locales,
    pub clock: Arc<dyn Clock>,
    pub settings: CommandSettings,
}

impl std::fmt::Debug for Deps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps").finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct CommandRouter {
    deps: Deps,
}

/// What a handler sees: the dependencies, the request and who is asking.
pub(crate) struct Cx<'a> {
    pub deps: &'a Deps,
    pub req: &'a CommandRequest,
    pub owner: bool,
}

impl Cx<'_> {
    pub fn t(&self, msg: &Msg) -> String {
        self.deps.locales.render(msg)
    }

    pub fn group(&self) -> GroupId {
        self.req.group
    }

    pub fn tokens(&self) -> Vec<&str> {
        self.req.args.split_whitespace().collect()
    }

    /// A member's name in this group: the name the platform shows for them now (read live),
    /// else their member number here, else (no record in the group) a label naming the account.
    pub async fn name_of(&self, account: AccountId) -> String {
        if let Some(name) = self
            .deps
            .directory
            .display_name(self.req.group, account)
            .await
            .filter(|n| !n.trim().is_empty())
        {
            return name;
        }
        match self.deps.admin.member_number(self.req.group, account).await {
            Ok(Some(number)) => self.t(&Msg::DisplayMember { number }),
            _ => self.t(&Msg::DisplayFallback {
                account: account.get().to_string(),
            }),
        }
    }

    pub fn is_owner(&self, account: AccountId) -> bool {
        self.deps.settings.owners.contains(&account)
    }
}

impl CommandRouter {
    pub fn new(deps: Deps) -> Self {
        Self { deps }
    }

    async fn dispatch(&self, command: Command, req: &CommandRequest) -> Result<String, Fail> {
        let owner = self.deps.settings.owners.contains(&req.sender);
        if command.owner_only() && !owner {
            return Err(Fail::Say(Msg::OwnerOnly {}));
        }
        let cx = Cx {
            deps: &self.deps,
            req,
            owner,
        };
        match command {
            Command::Help => handlers::help(&cx).await,
            Command::Who => handlers::who(&cx).await,
            Command::Note => handlers::note(&cx).await,
            Command::Name => handlers::name(&cx).await,
            Command::Forget => handlers::forget(&cx).await,
            Command::Link => handlers::link(&cx).await,
            Command::Unlink => handlers::unlink(&cx).await,
            Command::Group => handlers::group(&cx).await,
            Command::Runs => handlers::runs(&cx).await,
            Command::Logs => handlers::logs(&cx).await,
            Command::Mute => handlers::mute(&cx).await,
            Command::Block => handlers::block(&cx).await,
            Command::Members => handlers::members(&cx).await,
            Command::Stats => handlers::stats(&cx).await,
            Command::Top => handlers::top(&cx).await,
            Command::Tasks => handlers::tasks(&cx).await,
        }
    }

    /// The reply text for a request.
    pub async fn respond(&self, req: &CommandRequest) -> Option<String> {
        let command = Command::from_word(&req.word)?;
        let text = match self.dispatch(command, req).await {
            Ok(text) => text,
            Err(Fail::Say(msg)) => self.deps.locales.render(&msg),
            Err(Fail::Storage(error)) => {
                tracing::error!(%error, command = command.name(), "command failed in storage");
                self.deps.locales.render(&Msg::OutcomeUnknown {})
            }
        };
        Some(text)
    }
}

#[async_trait]
impl Commands for CommandRouter {
    fn recognizes(&self, word: &str) -> bool {
        Command::from_word(word).is_some()
    }

    async fn run(&self, request: CommandRequest) {
        let Some(text) = self.respond(&request).await else {
            return;
        };
        // Each piece quotes the command and mentions its sender, so a split reply stays attributed.
        let limit = self.deps.settings.max_message_chars.saturating_sub(2);
        for piece in reply::split(&text, limit) {
            let segments = vec![
                OutSegment::Reply(request.message),
                OutSegment::At(request.sender),
                OutSegment::Text(format!(" {piece}")),
            ];
            if let Err(error) = self.deps.delivery.send(request.group, segments).await {
                tracing::warn!(%error, command = %request.word, "could not deliver a command reply");
                return;
            }
        }
    }
}
