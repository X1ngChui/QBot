use qbot_i18n::Msg;

/// Every command. Names say what a command is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Everything the bot knows about a member or the group (names and statements, read from
    /// chat or entered by hand): viewing it, and adding, correcting, closing or forgetting an
    /// entry.
    Memory,
    Link,
    Unlink,
    Stats,
    Top,
    Tasks,
    Members,
    Block,
    Mute,
    /// The group's delegated admins.
    Admin,
    /// Recent runs and their steps.
    Runs,
    /// Recent warnings and errors.
    Logs,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Profile,
    Identity,
    Group,
    Admin,
    Help,
}

impl Command {
    /// In the order `/help` lists them; commands of one category are adjacent.
    pub const ALL: [Command; 13] = [
        Command::Memory,
        Command::Link,
        Command::Unlink,
        Command::Stats,
        Command::Top,
        Command::Tasks,
        Command::Members,
        Command::Block,
        Command::Mute,
        Command::Admin,
        Command::Runs,
        Command::Logs,
        Command::Help,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Command::Memory => "memory",
            Command::Link => "link",
            Command::Unlink => "unlink",
            Command::Stats => "stats",
            Command::Top => "top",
            Command::Tasks => "tasks",
            Command::Members => "members",
            Command::Block => "block",
            Command::Mute => "mute",
            Command::Admin => "admin",
            Command::Runs => "runs",
            Command::Logs => "logs",
            Command::Help => "help",
        }
    }

    /// Commands only owners may use at all. Some member commands have forms reserved to owners
    /// or delegated admins (`/link @a @b`, `/unlink @a`, changing what is known about the
    /// group), checked by their handlers.
    pub fn owner_only(self) -> bool {
        matches!(
            self,
            Command::Tasks
                | Command::Members
                | Command::Block
                | Command::Mute
                | Command::Admin
                | Command::Runs
                | Command::Logs
        )
    }

    /// Owner-only commands a bot owner's delegated admin of a group may also use there.
    pub fn delegated(self) -> bool {
        matches!(self, Command::Mute | Command::Block | Command::Members)
    }

    /// The commands a delegated admin is answered for in a muted group: those that manage who
    /// the bot answers, so the group can be unmuted.
    pub fn manages_admission(self) -> bool {
        matches!(self, Command::Mute | Command::Block)
    }

    /// Commands that act on a member (`@account`, else the sender), where a delegated admin
    /// may reach the group's ordinary members.
    pub fn targets_members(self) -> bool {
        matches!(self, Command::Memory)
    }

    pub fn category(self) -> Category {
        match self {
            Command::Memory => Category::Profile,
            Command::Link | Command::Unlink => Category::Identity,
            Command::Stats | Command::Top => Category::Group,
            Command::Tasks
            | Command::Members
            | Command::Block
            | Command::Mute
            | Command::Admin
            | Command::Runs
            | Command::Logs => Category::Admin,
            Command::Help => Category::Help,
        }
    }

    /// Exact, case-sensitive match of a typed word including its slash. This is how a message is
    /// recognised as a command at all.
    pub fn from_word(word: &str) -> Option<Command> {
        let name = word.strip_prefix('/')?;
        Command::ALL.into_iter().find(|c| c.name() == name)
    }

    /// Lenient lookup for `/help NAME`: leading slashes and case are ignored.
    pub fn find(name: &str) -> Option<Command> {
        let name = name.trim().trim_start_matches('/').to_lowercase();
        Command::ALL.into_iter().find(|c| c.name() == name)
    }

    pub fn what(self) -> Msg {
        match self {
            Command::Memory => Msg::WhatMemory {},
            Command::Link => Msg::WhatLink {},
            Command::Unlink => Msg::WhatUnlink {},
            Command::Stats => Msg::WhatStats {},
            Command::Top => Msg::WhatTop {},
            Command::Tasks => Msg::WhatTasks {},
            Command::Members => Msg::WhatMembers {},
            Command::Block => Msg::WhatBlock {},
            Command::Mute => Msg::WhatMute {},
            Command::Admin => Msg::WhatAdmin {},
            Command::Runs => Msg::WhatRuns {},
            Command::Logs => Msg::WhatLogs {},
            Command::Help => Msg::WhatHelp {},
        }
    }

    pub fn detail(self) -> Msg {
        match self {
            Command::Memory => Msg::DetailMemory {},
            Command::Link => Msg::DetailLink {},
            Command::Unlink => Msg::DetailUnlink {},
            Command::Stats => Msg::DetailStats {},
            Command::Top => Msg::DetailTop {},
            Command::Tasks => Msg::DetailTasks {},
            Command::Members => Msg::DetailMembers {},
            Command::Block => Msg::DetailBlock {},
            Command::Mute => Msg::DetailMute {},
            Command::Admin => Msg::DetailAdmin {},
            Command::Runs => Msg::DetailRuns {},
            Command::Logs => Msg::DetailLogs {},
            Command::Help => Msg::DetailHelp {},
        }
    }
}

impl Category {
    pub fn title(self) -> Msg {
        match self {
            Category::Profile => Msg::HelpCategoryProfile {},
            Category::Identity => Msg::HelpCategoryIdentity {},
            Category::Group => Msg::HelpCategoryGroup {},
            Category::Admin => Msg::HelpCategoryAdmin {},
            Category::Help => Msg::HelpCategoryHelp {},
        }
    }
}
