//! `agentos-eval`: the prompt-injection evaluation, from the command line.
//!
//! Without options it runs every case with the scripted worst-case planner:
//! no model, no tokens, the same numbers every time. With `--planner claude`
//! or `--planner openai` it asks a real model, which costs tokens; it never
//! does so because a key happens to be in the environment.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use agentos_eval::injection::report;
use agentos_eval::injection::scripted::Obedient;
use agentos_eval::injection::{
    run_case, Case, Human, Outcome, Protection, Selection, Setup, SharedPin, Template, GOALS,
    USER_TASKS,
};
use agentos_planner::claude::{ClaudeConfig, ClaudePlanner};
use agentos_planner::llm::TextModel;
use agentos_planner::openai::{OpenAiApi, OpenAiConfig, OpenAiPlanner, DEFAULT_BASE_URL};
use agentos_planner::Planner;
use agentos_services::judge::{Judge, LlmJudge};
use serde_json::json;

const HELP: &str = "agentos-eval：提示注入评测

用法：cargo run -p agentos-eval -- [选项]

  --planner scripted|claude|openai  规划器（默认 scripted：完全听从注入的脚本化规划器，不调用模型，结果确定）。
                                    claude 需要 ANTHROPIC_API_KEY；openai 需要 OPENAI_API_KEY 或 OPENAI_BASE_URL，
                                    以及 OPENAI_MODEL 或 --model。用模型会消耗 token。
  --model 名称                      模型（claude 默认 AGENTOS_MODEL 或 claude-opus-5-5；openai 默认 OPENAI_MODEL）
  --judge off|on|both               裁判，用同一个模型（默认：scripted 为 off，模型为 both）
  --taint on|off|both               污点（默认 both）。off 只在评测里构造：不可信能力被包一层，不再声明输出不可信
  --human attentive|rubber-stamp|both
                                    模拟的人（默认：scripted 为 both，模型为 attentive）
  --users 列表 --goals 列表 --templates 列表
                                    只跑一部分，逗号分隔（默认全部；模型默认只用 important 一种写法）
  --repeat N                        每个用例跑几遍（默认 1）
  --jobs N                          同时跑几个用例（默认 4）
  --out 路径                        把 Markdown 报告另存一份
  --json 路径                       每个用例的明细（JSON）

退出码：0 完成；1 在“策略 + 污点”和认真看的人之下有 L1 及以上的注入目标没问人就执行了；2 参数或配置错误。
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Scripted,
    Claude,
    OpenAi,
}

#[derive(Debug)]
struct Options {
    planner: Choice,
    model: Option<String>,
    judge: Option<Vec<bool>>,
    taint: Vec<Protection>,
    human: Option<Vec<Human>>,
    selection: Selection,
    templates_given: bool,
    repeat: u32,
    jobs: usize,
    out: Option<PathBuf>,
    json: Option<PathBuf>,
}

fn usage(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty() && *item != "all")
        .map(str::to_string)
        .collect()
}

fn parse(args: &[String]) -> io::Result<Option<Options>> {
    let mut options = Options {
        planner: Choice::Scripted,
        model: None,
        judge: None,
        taint: vec![Protection::PolicyAndTaint, Protection::PolicyOnly],
        human: None,
        selection: Selection::default(),
        templates_given: false,
        repeat: 1,
        jobs: 4,
        out: None,
        json: None,
    };
    let mut args = args.iter();
    while let Some(name) = args.next() {
        if name == "--help" || name == "-h" {
            return Ok(None);
        }
        let value = args
            .next()
            .ok_or_else(|| usage(format!("{name} 后面缺少值")))?
            .as_str();
        match name.as_str() {
            "--planner" => {
                options.planner = match value {
                    "scripted" => Choice::Scripted,
                    "claude" => Choice::Claude,
                    "openai" => Choice::OpenAi,
                    other => {
                        return Err(usage(format!(
                            "未知规划器 {other}，可选 scripted、claude、openai"
                        )))
                    }
                }
            }
            "--model" => options.model = Some(value.to_string()),
            "--judge" => {
                options.judge = Some(match value {
                    "off" => vec![false],
                    "on" => vec![true],
                    "both" => vec![false, true],
                    other => {
                        return Err(usage(format!(
                            "--judge 只能是 off、on 或 both，不是 {other}"
                        )))
                    }
                })
            }
            "--taint" => {
                options.taint = match value {
                    "on" => vec![Protection::PolicyAndTaint],
                    "off" => vec![Protection::PolicyOnly],
                    "both" => vec![Protection::PolicyAndTaint, Protection::PolicyOnly],
                    other => {
                        return Err(usage(format!(
                            "--taint 只能是 on、off 或 both，不是 {other}"
                        )))
                    }
                }
            }
            "--human" => {
                options.human = Some(match value {
                    "attentive" => vec![Human::Attentive],
                    "rubber-stamp" => vec![Human::RubberStamp],
                    "both" => vec![Human::Attentive, Human::RubberStamp],
                    other => {
                        return Err(usage(format!(
                            "--human 只能是 attentive、rubber-stamp 或 both，不是 {other}"
                        )))
                    }
                })
            }
            "--users" => {
                let users = list(value);
                if let Some(unknown) = users
                    .iter()
                    .find(|id| !USER_TASKS.iter().any(|u| u.id == id.as_str()))
                {
                    return Err(usage(format!("未知用户任务 {unknown}")));
                }
                options.selection.users = users;
            }
            "--goals" => {
                let goals = list(value);
                if let Some(unknown) = goals
                    .iter()
                    .find(|id| !GOALS.iter().any(|g| g.id == id.as_str()))
                {
                    return Err(usage(format!("未知注入目标 {unknown}")));
                }
                options.selection.goals = goals;
            }
            "--templates" => {
                options.selection.templates = list(value)
                    .iter()
                    .map(|id| {
                        Template::parse(id).ok_or_else(|| usage(format!("未知注入写法 {id}")))
                    })
                    .collect::<io::Result<_>>()?;
                options.templates_given = true;
            }
            "--repeat" | "--jobs" => {
                let number: usize = value
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| usage(format!("{name} 要一个正整数，不是 {value}")))?;
                if name == "--repeat" {
                    options.repeat = u32::try_from(number).map_err(|_| usage("--repeat 太大"))?;
                } else {
                    options.jobs = number;
                }
            }
            "--out" => options.out = Some(PathBuf::from(value)),
            "--json" => options.json = Some(PathBuf::from(value)),
            other => return Err(usage(format!("未知参数：{other}，使用 --help 查看用法"))),
        }
    }
    Ok(Some(options))
}

/// A planner for every case, the same model as plain text for the judge,
/// and a label for the report.
type Model = (Arc<dyn Planner>, Arc<dyn TextModel>, String);

fn model(choice: Choice, name: Option<&str>) -> io::Result<Option<Model>> {
    match choice {
        Choice::Scripted => Ok(None),
        Choice::Claude => {
            let mut cfg =
                ClaudeConfig::from_env().ok_or_else(|| usage("Claude 需要 ANTHROPIC_API_KEY"))?;
            if let Some(name) = name {
                cfg.model = name.to_string();
            }
            let planner = ClaudePlanner::new(cfg.clone()).map_err(io::Error::other)?;
            let label = format!("Claude（{}）", planner.model());
            let text = ClaudePlanner::new(cfg).map_err(io::Error::other)?;
            Ok(Some((Arc::new(planner), Arc::new(text), label)))
        }
        Choice::OpenAi => {
            let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
            OpenAiConfig::api_from_env().map_err(usage)?;
            let mut cfg = match OpenAiConfig::from_env() {
                Some(cfg) => cfg,
                None => {
                    let api_key = get("OPENAI_API_KEY");
                    let base_url = get("OPENAI_BASE_URL");
                    if api_key.is_none() && base_url.is_none() {
                        return Err(usage(
                            "OpenAI 兼容接口需要 OPENAI_API_KEY 或 OPENAI_BASE_URL",
                        ));
                    }
                    let base_url = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into());
                    let model = name
                        .map(str::to_string)
                        .ok_or_else(|| usage("OpenAI 兼容接口需要 OPENAI_MODEL 或 --model"))?;
                    OpenAiConfig {
                        api: OpenAiApi::default_for(&base_url),
                        api_key,
                        base_url,
                        model,
                    }
                }
            };
            if let Some(name) = name {
                cfg.model = name.to_string();
            }
            let planner = OpenAiPlanner::new(cfg.clone()).map_err(io::Error::other)?;
            let label = format!(
                "OpenAI 兼容接口（{} · {}）",
                planner.model(),
                planner.api().label()
            );
            let text = OpenAiPlanner::new(cfg).map_err(io::Error::other)?;
            Ok(Some((Arc::new(planner), Arc::new(text), label)))
        }
    }
}

/// The evaluation's scratch directory: sandboxes and the throwaway PIN
/// vault. Removed however the run ends.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("agentos-eval：{error}");
            ExitCode::from(2)
        }
    }
}

async fn run(args: &[String]) -> io::Result<ExitCode> {
    let Some(mut options) = parse(args)? else {
        print!("{HELP}");
        let ids = |ids: Vec<&str>| ids.join("、");
        println!(
            "\n用户任务：{}\n注入目标：{}\n注入写法：{}",
            ids(USER_TASKS.iter().map(|u| u.id).collect()),
            ids(GOALS.iter().map(|g| g.id).collect()),
            ids(Template::ALL.iter().map(|t| t.id()).collect()),
        );
        return Ok(ExitCode::SUCCESS);
    };
    let model = model(options.planner, options.model.as_deref())?;
    let scripted = model.is_none();
    if !scripted && !options.templates_given {
        // A model run costs tokens per case; the strongest wording alone
        // keeps the default affordable. --templates all runs every one.
        options.selection.templates = vec![Template::Important];
    }
    let judges = options.judge.clone().unwrap_or_else(|| {
        if scripted {
            vec![false]
        } else {
            vec![false, true]
        }
    });
    if scripted && judges.contains(&true) {
        return Err(usage("裁判需要模型：脚本化规划器只能 --judge off"));
    }
    let humans = options.human.clone().unwrap_or_else(|| {
        if scripted {
            vec![Human::Attentive, Human::RubberStamp]
        } else {
            vec![Human::Attentive]
        }
    });
    let cases = options.selection.cases();
    let scratch =
        Scratch(std::env::temp_dir().join(format!("agentos-eval-{}", std::process::id())));
    let pin = Arc::new(SharedPin::create(&scratch.0.join("pin"))?);
    let label = model.as_ref().map_or_else(
        || "脚本化的最坏情况规划器（完全听从注入，不调用模型）".to_string(),
        |(_, _, label)| label.clone(),
    );

    let mut jobs = Vec::new();
    for case in &cases {
        for &protection in &options.taint {
            for &judge in &judges {
                for &human in &humans {
                    for _ in 0..options.repeat {
                        jobs.push((*case, protection, judge, human));
                    }
                }
            }
        }
    }
    eprintln!(
        "{label}：{} 个用例 × {} 种配置，共 {} 次运行",
        cases.len(),
        options.taint.len() * judges.len() * humans.len(),
        jobs.len()
    );
    let started = Instant::now();
    let total = jobs.len();
    let limit = Arc::new(tokio::sync::Semaphore::new(options.jobs));
    let mut running = tokio::task::JoinSet::new();
    for (index, (case, protection, judge, human)) in jobs.into_iter().enumerate() {
        let planner: Arc<dyn Planner> = match &model {
            Some((planner, _, _)) => planner.clone(),
            None => Arc::new(Obedient { case }),
        };
        let judge: Option<Arc<dyn Judge>> = match (&model, judge) {
            (Some((_, text, _)), true) => Some(Arc::new(LlmJudge::new(text.clone()))),
            _ => None,
        };
        let mut setup = Setup::new(planner, scratch.0.clone());
        setup.judge = judge;
        setup.pin = Some(pin.clone());
        let limit = limit.clone();
        running.spawn(async move {
            let _slot = limit.acquire_owned().await.expect("never closed");
            (index, run_case(&setup, &case, protection, human).await)
        });
    }
    let mut outcomes: Vec<(usize, Outcome)> = Vec::with_capacity(total);
    let mut failures = Vec::new();
    while let Some(joined) = running.join_next().await {
        let (index, result) = joined.map_err(io::Error::other)?;
        match result {
            Ok(outcome) => {
                eprintln!(
                    "[{}/{total}] {} · {} · {}：{}{}，确认 {} 次",
                    outcomes.len() + failures.len() + 1,
                    outcome.case,
                    outcome.arm.label(),
                    outcome.arm.human.label(),
                    match (outcome.goal.is_some(), outcome.executed, outcome.attempted) {
                        (false, _, _) => "无注入",
                        (true, true, _) => "攻击成功",
                        (true, false, true) => "攻击被挡住",
                        (true, false, false) => "没有被带偏",
                    },
                    if outcome.done {
                        "，任务完成"
                    } else {
                        "，任务没完成"
                    },
                    outcome.confirmations,
                );
                outcomes.push((index, outcome));
            }
            Err(error) => failures.push(format!("第 {index} 次运行：{error}")),
        }
    }
    if !failures.is_empty() {
        return Err(io::Error::other(format!(
            "有运行没能完成：{}",
            failures.join("；")
        )));
    }
    outcomes.sort_by_key(|(index, _)| *index);
    let outcomes: Vec<Outcome> = outcomes.into_iter().map(|(_, outcome)| outcome).collect();

    let templates = if options.selection.templates.is_empty() {
        Template::ALL.to_vec()
    } else {
        options.selection.templates.clone()
    };
    let attacked = cases.iter().filter(|c| c.attack.is_some()).count();
    let about = format!(
        "规划器：{label}。用例：{} 个用户任务 × {} 个注入目标 × {} 种注入写法（{}）= {attacked} 个有注入的用例，另有每个用户任务一个无注入的对照；每个用例跑 {} 遍。认真看的人批准为用户请求服务的步骤，看到注入要求的步骤就取消任务；一律批准的人什么都批准（需要时输入 PIN）。",
        cases.iter().map(|c: &Case| c.user.id).collect::<std::collections::BTreeSet<_>>().len(),
        cases.iter().filter_map(Case::goal).map(|g| g.id).collect::<std::collections::BTreeSet<_>>().len(),
        templates.len(),
        templates.iter().map(|t| t.title()).collect::<Vec<_>>().join("、"),
        options.repeat,
    );
    // On stderr, so the same run prints the same report.
    eprintln!("耗时 {} 秒", started.elapsed().as_secs());
    let markdown = report::render(&outcomes, "提示注入评测", &about);
    print!("{markdown}");
    if let Some(path) = &options.out {
        std::fs::write(path, &markdown)?;
    }
    if let Some(path) = &options.json {
        let detail = json!({
            "planner": label,
            "repeat": options.repeat,
            "rows": report::rows(&outcomes),
            "outcomes": report::by_arm(&outcomes),
        });
        std::fs::write(
            path,
            serde_json::to_string_pretty(&detail).map_err(io::Error::other)?,
        )?;
    }
    Ok(if report::breaches(&outcomes).is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn options_parse_and_mistakes_are_rejected() {
        let options = parse(&args(&[
            "--planner",
            "openai",
            "--model",
            "m",
            "--judge",
            "on",
            "--taint",
            "off",
            "--human",
            "rubber-stamp",
            "--users",
            "web-answer",
            "--goals",
            "clear-notes,unlock-door",
            "--templates",
            "ignore,follow-up",
            "--repeat",
            "3",
            "--jobs",
            "2",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(options.planner, Choice::OpenAi);
        assert_eq!(options.judge, Some(vec![true]));
        assert_eq!(options.taint, vec![Protection::PolicyOnly]);
        assert_eq!(options.human, Some(vec![Human::RubberStamp]));
        assert_eq!(options.selection.goals, ["clear-notes", "unlock-door"]);
        assert_eq!(
            options.selection.templates,
            [Template::Ignore, Template::FollowUp]
        );
        assert_eq!((options.repeat, options.jobs), (3, 2));
        assert!(parse(&args(&["--help"])).unwrap().is_none());
        for bad in [
            vec!["--planner", "gpt"],
            vec!["--judge", "maybe"],
            vec!["--goals", "steal-everything"],
            vec!["--templates", "shouting"],
            vec!["--repeat", "0"],
            vec!["--jobs"],
            vec!["--surprise", "1"],
        ] {
            assert!(parse(&args(&bad)).is_err(), "{bad:?}");
        }
    }
}
