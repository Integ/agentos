use std::io;

use agentos_cli::args::{self, Action};
use agentos_cli::client::{Client, ClientError};
use agentos_cli::wait::wait_for_task;
use agentos_cli::{mcp, render, workbench};
use agentos_ipc::{Command, Reply};

#[tokio::main]
async fn main() -> io::Result<()> {
    let options = match args::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{}", render::safe_text(&error));
            std::process::exit(2)
        }
    };
    let client = Client::new(options.socket);
    let result = match options.action {
        Action::Help => {
            println!("{}", args::HELP);
            Ok(())
        }
        Action::Version => {
            println!("agentos {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Action::Workbench => return workbench::run(client).await,
        Action::Mcp { caller } => return mcp::serve(client.as_caller(caller)).await,
        Action::Request(mut command) => {
            if let (
                true,
                Command::Approve { pin, .. }
                | Command::Preauthorize { pin, .. }
                | Command::RegisterCaller { pin, .. },
            ) = (options.pin_stdin, &mut command)
            {
                eprint!("PIN：");
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                *pin = Some(line.trim().to_string());
            }
            request(&client, command, options.json).await
        }
        Action::SubmitAndWait {
            text,
            thread,
            timeout,
        } => {
            match submit_and_wait(&client, text, thread, timeout, options.json).await {
                Ok(0) => Ok(()),
                // The request itself worked; the status reports how the task
                // ended, which is not an error of this command.
                Ok(code) => std::process::exit(code),
                Err(error) => Err(error),
            }
        }
        Action::Watch(after) => watch(client, after, options.json).await,
    };
    if let Err(error) = result {
        if options.json {
            eprintln!("{}", error.json());
        } else {
            eprintln!("{}", render::safe_text(&error.to_string()));
        }
        std::process::exit(1);
    }
    Ok(())
}

/// Submit once, then only read until the task settles. JSON mode prints one
/// document: the final task, in the same shape as `agentos --json task`.
async fn submit_and_wait(
    client: &Client,
    text: String,
    thread: Option<agentos_runtime::TaskId>,
    timeout: std::time::Duration,
    json: bool,
) -> Result<i32, ClientError> {
    let accepted = client
        .request(Command::Submit {
            text,
            thread,
            spoken: false,
        })
        .await?;
    let Reply::Accepted { task } = &accepted else {
        unreachable!("the client verifies that a submit is answered with an acceptance")
    };
    if !json {
        eprintln!("[{}] 已接收，等待结果…", task.id);
    }
    match wait_for_task(client, task.id, timeout).await {
        Ok(outcome) => {
            if json {
                let reply = Reply::Task {
                    task: outcome.task().clone(),
                };
                println!(
                    "{}",
                    serde_json::to_string(&reply).expect("a task is serializable")
                );
            } else {
                println!("{}", render::wait_text(&outcome));
            }
            Ok(outcome.exit_code())
        }
        Err(error) => {
            // The task exists and keeps running; do not lose its number just
            // because watching it failed.
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&accepted).expect("a task is serializable")
                );
            } else {
                eprintln!(
                    "[{}] 已提交，但无法继续等待结果；任务仍由后台执行。",
                    task.id
                );
            }
            Err(error)
        }
    }
}

async fn request(client: &Client, command: Command, json: bool) -> Result<(), ClientError> {
    let reply = client.request(command).await?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&reply).expect("a reply is serializable")
        );
    } else {
        println!("{}", render::reply_text(&reply));
    }
    Ok(())
}

async fn watch(client: Client, after: Option<u64>, json: bool) -> Result<(), ClientError> {
    let mut stream = client.watch(after).await?;
    if !json {
        println!(
            "已订阅任务变更 · 快照 #{}。Ctrl-C 退出，不停止后台任务。",
            stream.revision
        );
    }
    // A dedicated read task prevents select cancellation from consuming a
    // partial event frame when Ctrl-C or other UI work wins a branch.
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let reader = tokio::spawn(async move {
        loop {
            let item = stream.next().await;
            let end = !matches!(&item, Ok(Some(_)));
            if tx.send(item).await.is_err() || end {
                break;
            }
        }
    });
    let result = loop {
        tokio::select! {
            item=rx.recv()=>match item {
                Some(Ok(Some(event)))=>{
                    if json {println!("{}",serde_json::to_string(&event).expect("event is serializable"));}
                    else {println!("{}",render::event_text(&event));}
                },
                Some(Err(error))=>break Err(error),
                _=>break Ok(()),
            },
            _=tokio::signal::ctrl_c()=>break Ok(()),
        }
    };
    reader.abort();
    result
}
