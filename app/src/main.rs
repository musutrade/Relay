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
    if args.get(1).map(String::as_str) == Some("adopt-review") {
        if args.len() != 6 {
            return Err("usage: relay-app adopt-review <config.json> <db-path> <predecessor-id> <request.json>".into());
        }
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&args[5])?;
        if !file.metadata()?.is_file() {
            return Err("adoption request must be a regular JSON file".into());
        }
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 {
            return Err("adoption request exceeds 16 KiB".into());
        }
        let request = serde_json::from_slice::<relay_app::ReviewAdoptionRequest>(&bytes)?;
        let id: i64 = args[4].parse()?;
        if id <= 0 {
            return Err("positive predecessor task id required".into());
        }
        let app = Application::open(&args[3], HostConfig::load(&args[2])?)?;
        println!(
            "{}",
            serde_json::to_string(&app.adopt_review(id, request)?)?
        );
        return Ok(());
    }
    if matches!(
        args.get(1).map(String::as_str),
        Some("auth-init" | "auth-password")
    ) {
        let initialize = args[1] == "auth-init";
        if args.len() != if initialize { 4 } else { 3 } {
            return Err("usage: relay-app auth-init <credentials.json> <username>\n       relay-app auth-password <credentials.json>".into());
        }
        relay_app::auth::initialize(
            std::path::Path::new(&args[2]),
            if initialize { Some(&args[3]) } else { None },
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("doctor") {
        let reconcile = args.len() == 4 && args[3] == "--confirm-catalog-stopped";
        if args.len() != 3 && !reconcile {
            return Err("usage: relay-app doctor <config.json> [--confirm-catalog-stopped]\nUse the confirmation flag only after inspecting the retained discovery process tree and confirming it has stopped.".into());
        }
        let host = relay_app::host::Host::new(HostConfig::load(&args[2])?)?;
        if reconcile {
            let cleared = relay_app::capabilities::confirm_discovery_stopped(&host)?;
            println!(
                "{}",
                serde_json::json!({"catalog_reconciled":true,"guard_cleared":cleared,"model_calls":false})
            );
            return Ok(());
        }
        let profiles: Vec<_> = host.config().native_agents.iter().map(|(name, profile)| {
            match host.probe_native(name, profile.native_permission == Some(relay_app::providers::NativePermission::ClaudeRestricted) || profile.native_sandboxed_review()) {
                Ok(probe) => serde_json::json!({"name":name,"compatible":true,"probe":probe,"authentication":"unknown","model_access":"unknown"}),
                Err(error) => serde_json::json!({"name":name,"compatible":false,"error":error.to_string(),"authentication":"unknown","model_access":"unknown"}),
            }
        }).collect();
        println!(
            "{}",
            serde_json::json!({"profiles":profiles,"model_calls":false,"authentication":"unknown"})
        );
        return Ok(());
    }
    if args.len() < 4 || args.len() > 5 || !matches!(args[1].as_str(), "serve" | "mcp") {
        return Err("usage: relay-app serve <config.json> <db-path> [127.0.0.1:8787]\n       relay-app mcp <config.json> <db-path>\n       relay-app doctor <config.json>".into());
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
    let token = std::env::var("RELAY_TOKEN").ok();
    let auth = match std::env::var_os("RELAY_AUTH_CONFIG") {
        Some(path) => relay_app::auth::Auth::load(std::path::Path::new(&path), token),
        None => relay_app::auth::Auth::new(None, token),
    }
    .map_err(|e| e.to_string())?;
    let router = relay_app::http::router_with_auth(Arc::clone(&app), auth);
    // Install both handlers before starting the worker. If registration fails,
    // no execution has begun and there is no claim to abandon.
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!(
        "Relay listening at http://{} (authentication required for /api)",
        listener.local_addr()?
    );
    let worker_app = Arc::clone(&app);
    let worker = std::thread::spawn(move || worker_app.worker());
    let signal_app = Arc::clone(&app);
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
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
