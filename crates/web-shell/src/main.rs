use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use agentos_cli::client::Client;
use agentos_web_shell::{handle_connection, Config, Options, HELP};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

#[tokio::main]
async fn main() -> io::Result<()> {
    if std::env::args()
        .skip(1)
        .any(|argument| argument == "--version" || argument == "-V")
    {
        println!("agentos-web-shell {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!("{HELP}");
            return Ok(());
        }
        Err(message) => {
            eprintln!("{message}\n\n{HELP}");
            std::process::exit(2);
        }
    };
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, options.port))).await?;
    let port = listener.local_addr()?.port();
    let config = Arc::new(Config::new(Client::new(options.socket), port)?);
    println!("AgentOS · 情景工作台");
    println!("在浏览器中打开（此链接包含本次启动的本地访问凭证，请勿分享）：");
    println!("http://127.0.0.1:{port}/#session={}", config.session());
    println!("Ctrl+C 关闭界面服务，后台任务继续运行。");
    let slots = Arc::new(Semaphore::new(64));
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let config = config.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = handle_connection(stream, &config).await;
                });
            }
            result = tokio::signal::ctrl_c() => {
                result?;
                break;
            }
        }
    }
    config.shutdown_voice().await;
    Ok(())
}
