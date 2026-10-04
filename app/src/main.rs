use relay_app::{Application, host::HostConfig};
use std::{net::SocketAddr, sync::Arc};

fn main() {
    if std::env::args().nth(1).as_deref() == Some("__relay_host_supervisor") {
        std::process::exit(relay_app::host::supervisor_main());
    }
    let runtime = tokio::runtime::Runtime::new().expect("create runtime");
    if let Err(error) = runtime.block_on(run()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 4 || args.len() > 5 || !matches!(args[1].as_str(), "serve" | "mcp") {
        return Err("usage: relay-app serve <config.json> <db-path> [127.0.0.1:8787]\n       relay-app mcp <config.json> <db-path>".into());
    }
    let config = HostConfig::load(&args[2])?;
    let app = Application::open(&args[3], config)?;
    if args[1] == "mcp" {
        if args.len() != 4 {
            return Err("mcp does not accept a bind address".into());
        }
        relay_app::mcp::serve(&app, std::io::stdin().lock(), std::io::stdout().lock())?;
        return Ok(());
    }
    let address: SocketAddr = args
        .get(4)
        .map(String::as_str)
        .unwrap_or("127.0.0.1:8787")
        .parse()?;
    if !address.ip().is_loopback() {
        return Err(
            "bind address must be loopback; use an authenticated TLS tunnel for remote access"
                .into(),
        );
    }
    let router = relay_app::http::router(
        Arc::clone(&app),
        std::env::var("RELAY_TOKEN").map_err(|_| "RELAY_TOKEN is required")?,
    )?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!(
        "Relay listening at http://{} (token required for /api)",
        listener.local_addr()?
    );
    let worker_app = Arc::clone(&app);
    let worker = std::thread::spawn(move || worker_app.worker());
    let signal_app = Arc::clone(&app);
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            signal_app.stop();
        })
        .await;
    app.stop();
    worker
        .join()
        .map_err(|_| "worker panicked; inspect active claim before restarting")?;
    served?;
    Ok(())
}
