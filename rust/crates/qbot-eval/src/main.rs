//! `qbot-eval`: reply-quality evaluation against the real text model.
//!
//! ```text
//! cargo run -p qbot-eval -- [--only NAME]... [--repeat N] [--persona FILE] [--scenarios DIR] [--out DIR]
//! ```
//!
//! Run from `rust/`. Each scenario in `eval/scenarios/*.toml` goes through the production prompt
//! layer, tool set and run loop; deterministic checks and an English LLM judge grade the result.
//! The report goes to `eval/results/<time>/` (`report.md`, `results.json`). The key is read from
//! `deploy/secrets/text_api_key` (or `QBOT_EVAL_SECRETS_DIR`) into memory only. Models:
//! `QBOT_EVAL_MODEL` (default `deepseek-flash`, the reply model) and `QBOT_EVAL_JUDGE_MODEL`
//! (default `deepseek-v4-pro`); `QBOT_EVAL_ENDPOINT` overrides the endpoint.

mod judge;
mod runner;
mod scenario;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use qbot_llm::responses::{KeySource, ReqwestTransport, ResponsesConfig, ResponsesProvider};
use qbot_prompt::{Persona, Personas};
use serde::Serialize;

use crate::judge::Verdict;
use crate::runner::{RunResult, Runner};
use crate::scenario::Scenario;

#[derive(Debug)]
struct Args {
    only: Vec<String>,
    repeat: u32,
    persona: Option<PathBuf>,
    scenarios: PathBuf,
    out: PathBuf,
}

fn args() -> Result<Args, String> {
    let mut args = Args {
        only: Vec::new(),
        repeat: 1,
        persona: None,
        scenarios: PathBuf::from("eval/scenarios"),
        out: PathBuf::from("eval/results"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--only" => args.only.push(value()?),
            "--repeat" => {
                args.repeat = value()?
                    .parse()
                    .map_err(|_| "--repeat needs a number".to_owned())?;
            }
            "--persona" => args.persona = Some(PathBuf::from(value()?)),
            "--scenarios" => args.scenarios = PathBuf::from(value()?),
            "--out" => args.out = PathBuf::from(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn provider(model: &str) -> Result<Arc<ResponsesProvider>, String> {
    let dir = std::env::var_os("QBOT_EVAL_SECRETS_DIR")
        .map_or_else(|| PathBuf::from("deploy/secrets"), PathBuf::from);
    let key_file = dir.join("text_api_key");
    let key = std::fs::read_to_string(&key_file)
        .map_err(|e| format!("cannot read {}: {}", key_file.display(), e.kind()))?
        .trim()
        .to_owned();
    let transport = ReqwestTransport::new(
        env_or("QBOT_EVAL_ENDPOINT", "https://api.deepseek.com"),
        KeySource::Static(key),
        Duration::from_secs(10),
    )
    .map_err(|e| e.to_string())?;
    let mut cfg = ResponsesConfig::deepseek(model);
    cfg.timeout = Some(Duration::from_secs(180));
    Ok(Arc::new(
        ResponsesProvider::new(cfg, Arc::new(transport)).map_err(|e| e.to_string())?,
    ))
}

/// The deployment's persona if there is one, else the shipped example.
fn persona(path: Option<&Path>) -> Result<Personas, String> {
    let path = match path {
        Some(p) => p.to_owned(),
        None => [
            "deploy/config/personas/default.toml",
            "deploy/config/personas/default.toml.example",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .ok_or("no persona file found; pass --persona")?,
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let persona: Persona = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Personas::single(persona))
}

fn scenarios(dir: &Path, only: &[String]) -> Result<Vec<(String, Scenario)>, String> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "toml") {
            continue;
        }
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !only.is_empty() && !only.contains(&name) {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{name}: {e}"))?;
        let scenario = Scenario::parse(&text).map_err(|e| format!("{name}: {e}"))?;
        found.push((name, scenario));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

#[derive(Debug, Serialize)]
struct Attempt {
    run: Option<RunResult>,
    /// Why the scenario could not be run or judged.
    failure: Option<String>,
    verdicts: Vec<Verdict>,
    evidence: String,
}

#[derive(Debug, Serialize)]
struct ScenarioReport {
    name: String,
    description: String,
    rubric: Vec<String>,
    attempts: Vec<Attempt>,
}

impl Attempt {
    fn checks_passed(&self) -> (usize, usize) {
        self.run.as_ref().map_or((0, 1), |r| {
            (r.checks.iter().filter(|c| c.passed).count(), r.checks.len())
        })
    }

    fn rubric_passed(&self, total: usize) -> (usize, usize) {
        (self.verdicts.iter().filter(|v| v.passed).count(), total)
    }
}

fn markdown(reports: &[ScenarioReport], model: &str, judge: &str) -> String {
    let mut out = format!(
        "# Reply-quality evaluation\n\nReply model `{model}`, judge `{judge}`.\n\n| Scenario | Run | Checks | Rubric |\n| --- | --- | --- | --- |\n"
    );
    for r in reports {
        for (i, a) in r.attempts.iter().enumerate() {
            let (cp, ct) = a.checks_passed();
            let (rp, rt) = a.rubric_passed(r.rubric.len());
            out.push_str(&format!(
                "| {} | {} | {cp}/{ct} | {rp}/{rt} |\n",
                r.name,
                i + 1
            ));
        }
    }
    for r in reports {
        out.push_str(&format!("\n## {}\n\n{}\n", r.name, r.description));
        for (i, a) in r.attempts.iter().enumerate() {
            out.push_str(&format!("\n### Run {}\n\n", i + 1));
            if let Some(failure) = &a.failure {
                out.push_str(&format!("**Not completed:** {failure}\n\n"));
            }
            if let Some(run) = &a.run {
                for c in &run.checks {
                    out.push_str(&format!(
                        "- {} {}{}\n",
                        if c.passed { "PASS" } else { "FAIL" },
                        c.what,
                        if c.detail.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", c.detail)
                        }
                    ));
                }
            }
            for v in &a.verdicts {
                let criterion = r.rubric.get(v.criterion - 1).map_or("", String::as_str);
                out.push_str(&format!(
                    "- {} {criterion}: {}\n",
                    if v.passed { "PASS" } else { "FAIL" },
                    v.reason
                ));
            }
            out.push_str(&format!("\n```text\n{}\n```\n", a.evidence.trim_end()));
        }
    }
    out
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("qbot-eval: {error}");
            ExitCode::from(2)
        }
    }
}

/// Whether everything passed.
async fn real_main() -> Result<bool, String> {
    let args = args()?;
    let model = env_or("QBOT_EVAL_MODEL", "deepseek-flash");
    let judge_model = env_or("QBOT_EVAL_JUDGE_MODEL", "deepseek-v4-pro");
    let runner = Runner {
        provider: provider(&model)?,
        personas: persona(args.persona.as_deref())?,
        params: Runner::default_params(),
    };
    let judge = provider(&judge_model)?;
    let scenarios = scenarios(&args.scenarios, &args.only)?;
    if scenarios.is_empty() {
        return Err("no scenarios matched".into());
    }

    let mut reports = Vec::new();
    let mut all_passed = true;
    for (name, scenario) in &scenarios {
        let mut attempts = Vec::new();
        for n in 1..=args.repeat.max(1) {
            println!("{name} (run {n})...");
            let attempt = match runner.run(scenario).await {
                Ok((run, lines)) => {
                    let evidence = judge::evidence(scenario, &run, &lines);
                    let (verdicts, failure) =
                        match judge::judge(judge.as_ref(), scenario, &evidence).await {
                            Ok(v) => (v, None),
                            Err(e) => (Vec::new(), Some(format!("judging failed: {e}"))),
                        };
                    Attempt {
                        run: Some(run),
                        failure,
                        verdicts,
                        evidence,
                    }
                }
                Err(e) => Attempt {
                    run: None,
                    failure: Some(e),
                    verdicts: Vec::new(),
                    evidence: String::new(),
                },
            };
            let (cp, ct) = attempt.checks_passed();
            let (rp, rt) = attempt.rubric_passed(scenario.expect.rubric.len());
            all_passed &= attempt.failure.is_none() && cp == ct && rp == rt;
            println!("  checks {cp}/{ct}, rubric {rp}/{rt}");
            attempts.push(attempt);
        }
        reports.push(ScenarioReport {
            name: name.clone(),
            description: scenario.description.clone(),
            rubric: scenario.expect.rubric.clone(),
            attempts,
        });
    }

    let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S").to_string();
    let dir = args.out.join(stamp);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(
        dir.join("report.md"),
        markdown(&reports, &model, &judge_model),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&reports).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!("report: {}", dir.join("report.md").display());
    Ok(all_passed)
}
