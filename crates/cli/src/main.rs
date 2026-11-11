use std::io;

use agentos_cli::args::{self, Action};
use agentos_cli::client::{Client, ClientError};
use agentos_cli::{render, workbench};

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
            println!("AgentOS {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Action::Workbench => return workbench::run(client).await,
        Action::Request(command) => match client.request(command).await {
            Ok(reply) => {
                if options.json {
                    println!("{}", serde_json::to_string(&reply)?);
                } else {
                    println!("{}", render::reply_text(&reply));
                }
                Ok(())
            }
            Err(error) => Err(error),
        },
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
