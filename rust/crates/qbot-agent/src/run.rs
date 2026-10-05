//! One agent run: the loop from trigger to end.
//!
//! The loop is deliberately small. It projects the transcript into the model's view, asks the
//! provider, appends the answer, executes the requested tools, appends the results, folds in new
//! chat, and repeats. It ends when the model stops calling tools, a tool ends the run, a limit
//! is hit, the deadline passes, the provider fails, or the run is cancelled.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use qbot_context::{ChatBatch, Item, Meta, RunEnd, Summary, ToolCall, Transcript, project};
use qbot_core::{Clock, GroupId, ItemSeq, RunId};
use qbot_llm::{
    Continuation, Conversation, ForcedToolChoice, LlmError, Provider, ReasoningEffort, Renderer,
    Request, ToolChoice, Usage,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::env::{
    Archive, ArchiveCursor, EnvError, OpenedContext, RecapWhen, RunLog, RunSummary, Trigger,
    UsageDetail, UsageEvent, UsageSink,
};
use crate::executor::{RunLimits, execute_turn};
use crate::tool::{ChatView, RunState, ToolCx, ToolSet};

/// Shared, immutable collaborators of every run.
pub struct RunDeps {
    pub provider: Arc<dyn Provider>,
    pub tools: ToolSet,
    pub archive: Arc<dyn Archive>,
    pub log: Arc<dyn RunLog>,
    pub sink: Arc<dyn UsageSink>,
    pub renderer: Arc<dyn Renderer>,
    pub clock: Arc<dyn Clock>,
    pub limits: RunLimits,
    /// The reasoning effort of a reply's model calls.
    pub reasoning: ReasoningEffort,
    /// Where image bytes for `Part::Image` keys in the conversation come from (pictures a tool
    /// opened). `None` when nothing can produce images.
    pub media: Option<Arc<dyn qbot_llm::MediaStore>>,
}

impl std::fmt::Debug for RunDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunDeps")
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct RunInput {
    pub run: RunId,
    pub group: GroupId,
    pub trigger: Trigger,
    pub context: OpenedContext,
    pub deadline: Instant,
}

/// What a finished run reports. The transcript is the complete, inspectable record.
#[derive(Debug)]
pub struct RunReport {
    pub run: RunId,
    pub group: GroupId,
    pub end: RunEnd,
    /// The provider error behind `RunEnd::ModelError`.
    pub error: Option<LlmError>,
    pub usage: Usage,
    pub turns: u32,
    pub tool_calls: u32,
    pub sends: u32,
    pub transcript: Transcript,
}

struct Run<'a> {
    deps: &'a RunDeps,
    input: &'a RunInput,
    cancel: &'a CancellationToken,
    transcript: Transcript,
    logged: usize,
    view: ChatView,
    state: RunState,
    cursor: ArchiveCursor,
    continuation: Option<Continuation>,
    usage: Usage,
    turns: u32,
    tool_calls: usize,
    error: Option<LlmError>,
    /// Whether the model was already told that its text was not delivered.
    nudged: bool,
    /// Reasoning was switched off for a forced turn; it stays off for the rest of the run, as
    /// `ForcedToolChoice::WithoutReasoning` requires.
    reasoning_off: bool,
    /// Chat items an episode recap replaces only if the provider finds the context too long.
    recaps: Vec<(ItemSeq, String)>,
}

pub async fn execute(deps: &RunDeps, input: &RunInput, cancel: &CancellationToken) -> RunReport {
    let mut run = Run {
        deps,
        input,
        cancel,
        transcript: Transcript::new(),
        logged: 0,
        view: ChatView::default(),
        state: RunState::default(),
        cursor: input.context.cursor,
        continuation: None,
        usage: Usage::ZERO,
        turns: 0,
        tool_calls: 0,
        error: None,
        recaps: Vec::new(),
        nudged: false,
        reasoning_off: false,
    };
    let mut end = run.drive().await.unwrap_or_else(|end| end);

    // Whatever happened, leave a well-formed transcript: unresolved calls are interrupted.
    run.transcript.close_interrupted();
    let _ = run.transcript.append(Item::Meta(Meta::RunEnded(end)));
    if run.persist().await.is_err() {
        end = RunEnd::Environment;
    }
    let summary = RunSummary {
        end,
        error: run.error.as_ref().map(LlmError::class),
        usage: run.usage,
        turns: run.turns,
        tool_calls: u32::try_from(run.tool_calls).unwrap_or(u32::MAX),
        sends: run.state.sends(),
    };
    if deps
        .log
        .finish(input.group, input.run, &summary)
        .await
        .is_err()
    {
        end = RunEnd::Environment;
    }
    deps.sink.record(UsageEvent {
        group: input.group,
        run: input.run,
        turn: run.turns,
        latency: Duration::ZERO,
        detail: UsageDetail::RunEnded {
            end,
            usage: run.usage,
            turns: run.turns,
            tool_calls: u32::try_from(run.tool_calls).unwrap_or(u32::MAX),
        },
    });
    RunReport {
        run: input.run,
        group: input.group,
        end,
        error: run.error,
        usage: run.usage,
        turns: run.turns,
        tool_calls: u32::try_from(run.tool_calls).unwrap_or(u32::MAX),
        sends: run.state.sends(),
        transcript: run.transcript,
    }
}

impl Run<'_> {
    /// `Ok` is a normal ending; `Err` is an abnormal one. Both are a [`RunEnd`].
    async fn drive(&mut self) -> Result<RunEnd, RunEnd> {
        self.open().await?;
        let specs = self.deps.tools.specs();
        let mut choice = ToolChoice::Auto;
        loop {
            self.turns += 1;
            let calls = self.model_turn(&specs, &choice).await?;
            choice = ToolChoice::Auto;
            if calls.is_empty() {
                // Text without a tool call was meant for the group but never reached it. Say so
                // once and require a tool on the next turn: send it, or stay silent explicitly.
                match &self.input.context.undelivered_note {
                    Some(note) if !self.nudged && self.wrote_text() => {
                        self.nudged = true;
                        self.append(Item::Instruction(note.clone()))?;
                        self.persist().await?;
                        choice = ToolChoice::Required;
                        if self.turns < self.deps.limits.max_turns {
                            continue;
                        }
                        return Ok(RunEnd::StepLimit);
                    }
                    _ => return Ok(RunEnd::Completed),
                }
            }
            let ends_run = self.tool_turn(&calls).await?;
            if ends_run {
                return Ok(if self.state.sends() > 0 {
                    RunEnd::Delivered
                } else {
                    RunEnd::Completed
                });
            }
            if self.turns >= self.deps.limits.max_turns {
                return Ok(RunEnd::StepLimit);
            }
            self.fold_arrivals().await?;
        }
    }

    async fn open(&mut self) -> Result<(), RunEnd> {
        let context = &self.input.context;
        for instruction in &context.instructions {
            self.append(Item::Instruction(instruction.clone()))?;
        }
        self.view.absorb(&context.window);
        // Lines an episode may stand in for get their own chat item, so a summary can later
        // replace exactly them.
        let mut start = 0;
        for recap in &context.recaps {
            if recap.lines.start > start {
                let lines = context.window[start..recap.lines.start].to_vec();
                self.append(Item::Chat(ChatBatch::new(lines)))?;
            }
            let lines = context.window[recap.lines.clone()].to_vec();
            let seq = self.append(Item::Chat(ChatBatch::new(lines)))?;
            match recap.when {
                RecapWhen::Open => {
                    self.append(Item::Summary(Summary {
                        from: seq,
                        to: ItemSeq::new(seq.get() + 1),
                        text: recap.text.clone(),
                    }))?;
                }
                RecapWhen::Overflow => self.recaps.push((seq, recap.text.clone())),
            }
            start = recap.lines.end;
        }
        if start < context.window.len() || context.recaps.is_empty() {
            let lines = context.window[start..].to_vec();
            self.append(Item::Chat(ChatBatch::new(lines)))?;
        }
        if let Some(note) = &context.trigger_note {
            self.append(Item::Instruction(note.clone()))?;
        }
        self.persist().await
    }

    /// Whether the last model turn wrote any text.
    fn wrote_text(&self) -> bool {
        self.transcript
            .items()
            .iter()
            .rev()
            .find_map(|item| match item {
                Item::Assistant(turn) => Some(!turn.text().trim().is_empty()),
                _ => None,
            })
            == Some(true)
    }

    async fn model_turn(
        &mut self,
        specs: &[qbot_llm::ToolSpec],
        choice: &ToolChoice,
    ) -> Result<Vec<ToolCall>, RunEnd> {
        let info = self.deps.provider.info().clone();
        // A provider that forces tools only without reasoning gets this one turn without it.
        let mut reasoning = self.deps.reasoning;
        if *choice != ToolChoice::Auto
            && info.capabilities.forced_tool_choice == ForcedToolChoice::WithoutReasoning
        {
            self.reasoning_off = true;
        }
        if self.reasoning_off {
            reasoning = ReasoningEffort::Off;
        }
        let choice = if *choice != ToolChoice::Auto
            && info.capabilities.forced_tool_choice == ForcedToolChoice::Never
        {
            ToolChoice::Auto
        } else {
            choice.clone()
        };
        let (answer, latency) = loop {
            let view = project(&self.transcript).map_err(|_| RunEnd::Environment)?;
            let conversation = Conversation::lower(&view, &*self.deps.renderer);
            let request = Request {
                conversation: &conversation,
                tools: specs,
                tool_choice: choice.clone(),
                parallel_tool_calls: true,
                reasoning,
                continuation: self.continuation.as_ref(),
                media: self.deps.media.as_deref(),
            };
            let started = Instant::now();
            let answer = self.bounded(self.deps.provider.respond(request)).await?;
            let latency = started.elapsed();
            match answer {
                Err(error @ LlmError::ContextTooLong) if !self.recaps.is_empty() => {
                    self.record_model(&info, None, 0, Some(error.class()), latency);
                    self.compact().await?;
                }
                answer => break (answer, latency),
            }
        };
        let response = match answer {
            Ok(response) => response,
            Err(error) => {
                self.record_model(&info, None, 0, Some(error.class()), latency);
                self.error = Some(error);
                return Err(RunEnd::ModelError);
            }
        };
        self.record_model(
            &info,
            Some(response.usage),
            response.meta.attempts,
            None,
            latency,
        );
        self.usage += response.usage;
        self.continuation = Some(response.continuation);

        let calls: Vec<ToolCall> = response.turn.calls().cloned().collect();
        if let Err(error) = self.transcript.append(Item::Assistant(response.turn)) {
            // The provider produced a turn the transcript cannot hold (for example a duplicate
            // call id). That is a contract violation by the provider, not by us.
            self.error = Some(LlmError::Protocol(error.to_string()));
            return Err(RunEnd::ModelError);
        }
        self.persist().await?;
        Ok(calls)
    }

    /// Execute the turn's calls. Returns whether a tool ended the run.
    async fn tool_turn(&mut self, calls: &[ToolCall]) -> Result<bool, RunEnd> {
        let cx = ToolCx {
            group: self.input.group,
            run: self.input.run,
            trigger: &self.input.trigger,
            view: &self.view,
            state: &self.state,
            clock: &*self.deps.clock,
        };
        let executed = {
            let fut = execute_turn(&self.deps.tools, calls, &cx);
            // Cancellation or the deadline drops the futures; the calls stay pending and are
            // closed as interrupted when the run ends.
            let deadline = self.input.deadline;
            let cancel = self.cancel;
            tokio::select! {
                () = cancel.cancelled() => return Err(RunEnd::Cancelled),
                result = tokio::time::timeout_at(deadline, fut) => result.map_err(|_| RunEnd::Deadline)?,
            }
        };
        self.tool_calls += calls.len();
        let mut ends_run = false;
        for item in executed {
            self.deps.sink.record(UsageEvent {
                group: self.input.group,
                run: self.input.run,
                turn: self.turns,
                latency: item.latency,
                detail: UsageDetail::Tool {
                    name: item.name.clone(),
                    outcome: item.result.outcome,
                },
            });
            ends_run |= item.ends_run;
            self.append(Item::ToolResult(item.result))?;
        }
        self.persist().await?;
        Ok(ends_run)
    }

    /// Fold group lines archived since the last look into the run, except messages the model
    /// has already been told about through a tool result (such as the echo of its own send).
    async fn fold_arrivals(&mut self) -> Result<(), RunEnd> {
        let arrived = self
            .bounded(self.deps.archive.since(self.input.group, self.cursor))
            .await?
            .map_err(|_| RunEnd::Environment)?;
        let Some(last) = arrived.last() else {
            return Ok(());
        };
        self.cursor = last.seq;
        let lines: Vec<_> = arrived
            .into_iter()
            .map(|a| a.line)
            .filter(|line| !self.state.is_observed(line.message))
            .collect();
        if lines.is_empty() {
            return Ok(());
        }
        self.view.absorb(&lines);
        self.append(Item::Chat(ChatBatch::new(lines)))?;
        self.persist().await
    }

    /// The fallback when the context is too long despite the history policy: replace the raw
    /// chat items episodes cover with their recaps, once. The originals stay in the transcript.
    async fn compact(&mut self) -> Result<(), RunEnd> {
        for (seq, text) in std::mem::take(&mut self.recaps) {
            self.append(Item::Summary(Summary {
                from: seq,
                to: ItemSeq::new(seq.get() + 1),
                text,
            }))?;
        }
        self.persist().await
    }

    fn append(&mut self, item: Item) -> Result<ItemSeq, RunEnd> {
        self.transcript
            .append(item)
            .map_err(|_| RunEnd::Environment)
    }

    async fn persist(&mut self) -> Result<(), RunEnd> {
        while self.logged < self.transcript.len() {
            let seq = ItemSeq::new(u32::try_from(self.logged).map_err(|_| RunEnd::Environment)?);
            let item = &self.transcript.items()[self.logged];
            self.deps
                .log
                .append(self.input.group, self.input.run, seq, item)
                .await
                .map_err(|_: EnvError| RunEnd::Environment)?;
            self.logged += 1;
        }
        Ok(())
    }

    /// Await `future` unless the run is cancelled or past its deadline.
    async fn bounded<F: Future>(&self, future: F) -> Result<F::Output, RunEnd> {
        tokio::select! {
            () = self.cancel.cancelled() => Err(RunEnd::Cancelled),
            result = tokio::time::timeout_at(self.input.deadline, future) => result.map_err(|_| RunEnd::Deadline),
        }
    }

    fn record_model(
        &self,
        info: &qbot_llm::ProviderInfo,
        usage: Option<Usage>,
        attempts: u32,
        error: Option<&'static str>,
        latency: Duration,
    ) {
        self.deps.sink.record(UsageEvent {
            group: self.input.group,
            run: self.input.run,
            turn: self.turns,
            latency,
            detail: UsageDetail::Model {
                provider: info.id.as_str().to_owned(),
                model: info.model.clone(),
                usage,
                attempts,
                error,
            },
        });
    }
}
