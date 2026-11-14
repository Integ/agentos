//! A live, line-oriented terminal workbench. Views update incrementally without
//! clearing the terminal or discarding partially typed input.

use std::io::{self, BufRead, Write};
use std::time::Duration;

use agentos_core::now_ms;
use agentos_ipc::{Command, Event, Reply, TaskSummary};
use agentos_runtime::{Task, TaskId, TaskState};
use tokio::sync::mpsc;

use crate::args::{parse_workbench, WorkbenchCommand, WORKBENCH_HELP};
use crate::client::{Client, ClientError};
use crate::render;

const PAGE_SIZE: usize = 12;

pub fn approval_command(
    displayed: Option<&Task>,
    id: TaskId,
    step: usize,
    at_ms: u64,
) -> Result<Command, String> {
    let task = displayed
        .filter(|task| task.id == id)
        .ok_or("请先用 /task 选择并查看这个任务的具体确认步骤")?;
    let confirmation = task
        .confirmation
        .as_ref()
        .filter(|c| {
            task.state == TaskState::AwaitingConfirmation
                && c.step + 1 == step
                && at_ms < c.expires_at_ms
                && !c.token.is_empty()
        })
        .ok_or("当前展示的确认请求无效或已过期；请刷新并核对任务与步骤")?;
    if !render::confirmation_visible(task) {
        return Err("待批准参数过长，工作台未完整展示；请用 agentos --json task 查看完整参数，再通过独立 approve 命令显式提供令牌".into());
    }
    if confirmation.verify_identity {
        return Err(format!(
            "这一步不可逆，需要 PIN。工作台不读取 PIN；请在另一个终端运行：\n  agentos approve {id} {step} --confirmation <令牌> --pin-stdin\n令牌用 agentos task {id} 查看。"
        ));
    }
    Ok(Command::Approve {
        id,
        step,
        confirmation: confirmation.token.clone(),
        pin: None,
    })
}

struct View {
    tasks: Vec<TaskSummary>,
    selected: Option<Task>,
    selected_id: Option<TaskId>,
    before: Option<TaskId>,
    previous: Vec<Option<TaskId>>,
    next_before: Option<TaskId>,
    revision: u64,
    connected: bool,
    /// The conversation plain input continues; `None` starts a new one.
    thread: Option<TaskId>,
}

impl View {
    fn new() -> Self {
        Self {
            tasks: Vec::new(),
            selected: None,
            selected_id: None,
            before: None,
            previous: Vec::new(),
            next_before: None,
            revision: 0,
            connected: false,
            thread: None,
        }
    }

    fn disconnect(&mut self) {
        self.connected = false;
        self.selected = None;
    }

    async fn refresh(&mut self, client: &Client) -> Result<(), ClientError> {
        // A failed refresh cannot leave an old approval authorization active.
        self.selected = None;
        let Reply::Tasks {
            tasks,
            next_before,
            revision,
        } = client
            .request(Command::List {
                before: self.before,
                limit: PAGE_SIZE,
                state: None,
                thread: None,
            })
            .await
            .inspect_err(|_| self.disconnect())?
        else {
            return Err(ClientError::Protocol {
                message: "后台未返回任务页".into(),
                ambiguous: false,
            });
        };
        self.tasks = tasks;
        self.next_before = next_before;
        self.revision = revision;
        self.connected = true;
        if self.selected_id.is_none() {
            self.selected_id = self.tasks.first().map(|task| task.id);
        }
        if let Some(id) = self.selected_id {
            match client.request(Command::Get { id }).await {
                Ok(Reply::Task { task }) => self.selected = Some(task),
                Err(ClientError::Remote(error)) if error.code == "not_found" => {
                    self.selected = None;
                    self.selected_id = None;
                }
                Err(error) => {
                    self.disconnect();
                    return Err(error);
                }
                _ => {
                    return Err(ClientError::Protocol {
                        message: "后台未返回选中任务".into(),
                        ambiguous: false,
                    })
                }
            }
        }
        Ok(())
    }

    fn show(&self) {
        println!(
            "{}",
            render::dashboard(
                &self.tasks,
                self.selected.as_ref(),
                self.revision,
                self.connected,
                self.previous.len() + 1,
                self.next_before.is_some()
            )
        );
        prompt();
    }
}

fn prompt() {
    print!("意图 > ");
    let _ = io::stdout().flush();
}

pub fn stdin_lines() -> mpsc::Receiver<io::Result<String>> {
    let (tx, rx) = mpsc::channel(16);
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let failed = line.is_err();
            if tx.blocking_send(line).is_err() || failed {
                break;
            }
        }
    });
    rx
}

enum Notice {
    Connected,
    Changed(Event),
    Disconnected(String),
}

/// Only this task consumes event frames. Cancelling a UI select branch must
/// never cancel a partially consumed newline-delimited frame.
async fn follow_changes(client: Client, tx: mpsc::Sender<Notice>) {
    let mut cursor = None;
    let mut reported_down = false;
    let mut delay = Duration::from_millis(500);
    loop {
        match client.watch(cursor).await {
            Ok(mut stream) => {
                cursor = Some(stream.revision);
                delay = Duration::from_millis(500);
                if tx.send(Notice::Connected).await.is_err() {
                    return;
                }
                let failure = loop {
                    match stream.next().await {
                        Ok(Some(event)) => {
                            // Resync can legitimately reset the cursor after a
                            // state restore or endpoint replacement.
                            cursor = Some(event.revision);
                            if tx.send(Notice::Changed(event)).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => break "后台连接已关闭".into(),
                        Err(error) => break error.to_string(),
                    }
                };
                if tx.send(Notice::Disconnected(failure)).await.is_err() {
                    return;
                }
                reported_down = true;
            }
            Err(error) => {
                if !reported_down {
                    if tx
                        .send(Notice::Disconnected(error.to_string()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    reported_down = true;
                }
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

fn request_error(view: &mut View, error: ClientError) -> String {
    if !matches!(&error, ClientError::Remote(_)) {
        view.disconnect();
    }
    error.to_string()
}

async fn perform(
    view: &mut View,
    client: &Client,
    action: WorkbenchCommand,
) -> Result<bool, String> {
    match action {
        WorkbenchCommand::Quit => return Ok(false),
        WorkbenchCommand::Help => println!("\n{WORKBENCH_HELP}"),
        WorkbenchCommand::Refresh => {}
        WorkbenchCommand::Next => {
            if let Some(before) = view.next_before {
                view.previous.push(view.before);
                view.before = Some(before);
                view.selected_id = None;
                view.selected = None;
            } else {
                println!("\n已到最早一页");
            }
        }
        WorkbenchCommand::Prev => {
            if let Some(before) = view.previous.pop() {
                view.before = before;
                view.selected_id = None;
                view.selected = None;
            } else {
                println!("\n已在第一页");
            }
        }
        WorkbenchCommand::Latest => {
            view.before = None;
            view.previous.clear();
            view.selected_id = None;
            view.selected = None;
        }
        WorkbenchCommand::Task(id) => {
            view.selected_id = Some(id);
            view.selected = None;
        }
        WorkbenchCommand::Submit(text) => {
            let reply = client
                .request(Command::Submit {
                    text,
                    thread: view.thread,
                    spoken: false,
                })
                .await
                .map_err(|e| request_error(view, e))?;
            if let Reply::Accepted { task } = &reply {
                view.thread = Some(task.thread);
                view.selected_id = Some(task.id);
                view.selected = Some(task.clone());
                view.before = None;
                view.previous.clear();
            }
            println!("\n{}", render::reply_text(&reply));
        }
        WorkbenchCommand::Cancel(id) => {
            let reply = client
                .request(Command::Cancel { id })
                .await
                .map_err(|e| request_error(view, e))?;
            println!("\n{}", render::reply_text(&reply));
        }
        WorkbenchCommand::Approve { id, step } => {
            if !view.connected {
                return Err("后台连接中断，请等视图恢复后重新查看具体确认请求".into());
            }
            let command = approval_command(view.selected.as_ref(), id, step, now_ms())?;
            let reply = client
                .request(command)
                .await
                .map_err(|e| request_error(view, e))?;
            // Do not retain a displayed authorization for a second local send.
            if let Some(task) = &mut view.selected {
                task.confirmation = None;
            }
            println!("\n{}", render::reply_text(&reply));
        }
        WorkbenchCommand::Reply { id, step, text } => {
            // The service checks the reply against the question the task is
            // waiting on now; a stale one is refused there, not guessed here.
            let reply = client
                .request(Command::Reply {
                    id,
                    step,
                    text,
                    spoken: false,
                })
                .await
                .map_err(|e| request_error(view, e))?;
            println!("\n{}", render::reply_text(&reply));
        }
        WorkbenchCommand::New => {
            view.thread = None;
            println!("\n下一句话开始一段新的对话。");
        }
        WorkbenchCommand::Capabilities => {
            let reply = client
                .request(Command::Capabilities {})
                .await
                .map_err(|e| request_error(view, e))?;
            println!("\n{}", render::reply_text(&reply));
        }
    }
    view.refresh(client).await.map_err(|e| e.to_string())?;
    Ok(true)
}

pub async fn run(client: Client) -> io::Result<()> {
    let mut view = View::new();
    if let Err(error) = view.refresh(&client).await {
        eprintln!("{}", render::safe_text(&error.to_string()));
    }
    println!(
        "工作台独立连接 {}。/help 查看操作；退出后后台任务继续。",
        render::safe_text(&client.path().display().to_string())
    );
    view.show();
    let (notice_tx, mut notices) = mpsc::channel(8);
    let follower = tokio::spawn(follow_changes(client.clone(), notice_tx));
    let mut lines = stdin_lines();
    loop {
        tokio::select! {
            line=lines.recv() => {
                let Some(line)=line else {break};
                let line=match line {Ok(line)=>line,Err(error)=>{eprintln!("{}",render::safe_text(&error.to_string()));break}};
                match parse_workbench(&line) {
                    Ok(action)=>match perform(&mut view,&client,action).await {
                        Ok(false)=>break,
                        Ok(true)=>view.show(),
                        Err(error)=>{eprintln!("\n{}",render::safe_text(&error));prompt();}
                    },
                    Err(error)=>{eprintln!("\n{}",render::safe_text(&error));prompt();}
                }
            }
            notice=notices.recv() => {
                let Some(notice)=notice else {break};
                let mut batch=vec![notice];
                // Bound visual churn while preserving authoritative refreshes.
                tokio::time::sleep(Duration::from_millis(100)).await;
                while let Ok(notice)=notices.try_recv() {batch.push(notice);}
                let mut changed=false;
                for notice in batch {
                    match notice {
                        Notice::Connected=>{view.connected=true;changed=true;},
                        Notice::Changed(event)=>{view.revision=event.revision;changed=true;},
                        Notice::Disconnected(error)=>{view.disconnect();println!("\n连接中断：{}。正在重连视图，不会重发操作。",render::safe_text(&error));},
                    }
                }
                if changed && view.connected {
                    if let Err(error)=view.refresh(&client).await {view.disconnect();eprintln!("\n{}",render::safe_text(&error.to_string()));}
                }
                view.show();
            }
            _=tokio::signal::ctrl_c()=>break,
        }
    }
    follower.abort();
    println!("\n已关闭工作台；后台服务和已接收任务继续运行。");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{CapabilityName, DeviceId, Intent, IntentId, Urgency};
    use agentos_runtime::{Confirmation, TaskStep, TaskStore};
    use serde_json::json;

    fn pending(args: serde_json::Value) -> Task {
        let store = TaskStore::memory();
        let task = store
            .create(Intent {
                id: IntentId(0),
                source: DeviceId::new("test"),
                speaker: None,
                text: "测试".into(),
                urgency: Urgency::Normal,
                at_ms: now_ms(),
            })
            .unwrap();
        store
            .update(task.id, |task| {
                task.state = TaskState::AwaitingConfirmation;
                task.steps = vec![TaskStep::pending(CapabilityName::new("mail.send"), args)];
                task.confirmation = Some(Confirmation {
                    step: 0,
                    expires_at_ms: now_ms() + 60_000,
                    token: "shown-token".into(),
                    verify_identity: false,
                });
            })
            .unwrap()
    }

    #[test]
    fn approval_uses_only_the_displayed_unexpired_confirmation() {
        let task = pending(json!({"to":"alice","body":"hi"}));
        assert!(approval_command(None, task.id, 1, now_ms()).is_err());
        assert!(approval_command(Some(&task), TaskId(2), 1, now_ms()).is_err());
        assert!(approval_command(Some(&task), task.id, 2, now_ms()).is_err());
        assert!(approval_command(Some(&task), task.id, 1, u64::MAX).is_err());
        let Command::Approve { confirmation, .. } =
            approval_command(Some(&task), task.id, 1, now_ms()).unwrap()
        else {
            panic!("not approval")
        };
        assert_eq!(confirmation, "shown-token");
    }

    #[test]
    fn hidden_confirmation_parameters_cannot_be_approved_from_the_workbench() {
        let task = pending(json!({"body":"x".repeat(10_000)}));
        let view = render::task_detail(&task, false);
        assert!(view.contains("参数过长"));
        assert!(!view.contains("批准 /approve"));
        assert!(approval_command(Some(&task), task.id, 1, now_ms()).is_err());
        let full = render::task_detail(&task, true);
        assert!(full.contains(&"x".repeat(10_000)));
    }
}
