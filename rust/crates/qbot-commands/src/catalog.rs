use qbot_i18n::Msg;

/// Every command. Names say what a command is about; related things that are kept apart (notes
/// people write, memory the bot learned) have separate commands, so no command acts on both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Everything on record about a member.
    Who,
    /// Notes people wrote by hand.
    Note,
    /// The names a member is called by.
    Name,
    /// Removes what the bot learned from chat (member facts, group knowledge).
    Forget,
    Link,
    Unlink,
    /// What the bot learned about the group.
    Group,
    Stats,
    Top,
    Tasks,
    Members,
    Block,
    Mute,
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
    pub const ALL: [Command; 16] = [
        Command::Who,
        Command::Note,
        Command::Name,
        Command::Forget,
        Command::Link,
        Command::Unlink,
        Command::Group,
        Command::Stats,
        Command::Top,
        Command::Tasks,
        Command::Members,
        Command::Block,
        Command::Mute,
        Command::Runs,
        Command::Logs,
        Command::Help,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Command::Who => "who",
            Command::Note => "note",
            Command::Name => "name",
            Command::Forget => "forget",
            Command::Link => "link",
            Command::Unlink => "unlink",
            Command::Group => "group",
            Command::Stats => "stats",
            Command::Top => "top",
            Command::Tasks => "tasks",
            Command::Members => "members",
            Command::Block => "block",
            Command::Mute => "mute",
            Command::Runs => "runs",
            Command::Logs => "logs",
            Command::Help => "help",
        }
    }

    /// Commands only owners may use at all. Some member commands have owner-only forms
    /// (`/link @a @b`, `/unlink @a`, `/forget group N`), checked by their handlers.
    pub fn owner_only(self) -> bool {
        matches!(
            self,
            Command::Tasks
                | Command::Members
                | Command::Block
                | Command::Mute
                | Command::Runs
                | Command::Logs
        )
    }

    pub fn category(self) -> Category {
        match self {
            Command::Who | Command::Note | Command::Name | Command::Forget => Category::Profile,
            Command::Link | Command::Unlink => Category::Identity,
            Command::Group | Command::Stats | Command::Top => Category::Group,
            Command::Tasks
            | Command::Members
            | Command::Block
            | Command::Mute
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
            Command::Who => Msg::WhatWho {},
            Command::Note => Msg::WhatNote {},
            Command::Name => Msg::WhatName {},
            Command::Forget => Msg::WhatForget {},
            Command::Link => Msg::WhatLink {},
            Command::Unlink => Msg::WhatUnlink {},
            Command::Group => Msg::WhatGroup {},
            Command::Stats => Msg::WhatStats {},
            Command::Top => Msg::WhatTop {},
            Command::Tasks => Msg::WhatTasks {},
            Command::Members => Msg::WhatMembers {},
            Command::Block => Msg::WhatBlock {},
            Command::Mute => Msg::WhatMute {},
            Command::Runs => Msg::WhatRuns {},
            Command::Logs => Msg::WhatLogs {},
            Command::Help => Msg::WhatHelp {},
        }
    }

    pub fn detail(self) -> Msg {
        match self {
            Command::Who => Msg::DetailWho {},
            Command::Note => Msg::DetailNote {},
            Command::Name => Msg::DetailName {},
            Command::Forget => Msg::DetailForget {},
            Command::Link => Msg::DetailLink {},
            Command::Unlink => Msg::DetailUnlink {},
            Command::Group => Msg::DetailGroup {},
            Command::Stats => Msg::DetailStats {},
            Command::Top => Msg::DetailTop {},
            Command::Tasks => Msg::DetailTasks {},
            Command::Members => Msg::DetailMembers {},
            Command::Block => Msg::DetailBlock {},
            Command::Mute => Msg::DetailMute {},
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
