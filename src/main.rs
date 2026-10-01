use anyhow::Context;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tvp_vod_proxy::{m3u, server, tvp};
use url::Url;

const USAGE: &str = "\
usage:
  tvp-vod-proxy [serve]             start the proxy (default)
  tvp-vod-proxy make-m3u HOST:PORT  print an M3U of all playable channels
  tvp-vod-proxy -h | --help         show this help

environment:
  BIND         listen address (default 0.0.0.0)
  PORT         listen port (default 8080)
  MAX_STREAMS  maximum concurrent streams, 0 = no limit (default 10)
  MAX_BITRATE  highest average variant bitrate in bit/s, 0 = no limit (default 0)
  FFMPEG       ffmpeg binary (default ffmpeg)
  TVP_API      API base URL (default https://vod.tvp.pl)
  RUST_LOG     log filter (default info)";

fn env<T: std::str::FromStr>(name: &str, default: T) -> anyhow::Result<T>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(v) => v
            .parse()
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("invalid {name}: {v}")),
        Err(_) => Ok(default),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let api = Url::parse(&env("TVP_API", tvp::DEFAULT_API.to_string())?)?;
    let client = tvp::Client::new(api, env("MAX_BITRATE", 0u64)?)?;
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["serve"] => serve(client).await,
        ["make-m3u", host] => make_m3u(client, host).await,
        ["-h" | "--help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => anyhow::bail!("{USAGE}"),
    }
}

async fn serve(client: tvp::Client) -> anyhow::Result<()> {
    let bind: IpAddr = env("BIND", IpAddr::from([0, 0, 0, 0]))?;
    let port: u16 = env("PORT", 8080)?;
    let max_streams = match env("MAX_STREAMS", 10usize)? {
        0 => Semaphore::MAX_PERMITS,
        n => n.min(Semaphore::MAX_PERMITS),
    };
    let shutdown = CancellationToken::new();
    let state = server::AppState {
        client,
        ffmpeg: PathBuf::from(env("FFMPEG", "ffmpeg".to_string())?),
        work_dir: std::env::temp_dir(),
        retry_delay: Duration::from_secs(1),
        quick_failure: Duration::from_secs(30),
        streams: Arc::new(Semaphore::new(max_streams)),
        shutdown: shutdown.clone(),
    };
    let listener = tokio::net::TcpListener::bind((bind, port)).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, server::router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            // end running streams, or graceful shutdown would wait for them forever
            shutdown.cancel();
        })
        .await?;
    Ok(())
}

/// Resolves on Ctrl-C (SIGINT) or SIGTERM (`docker stop`).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

async fn make_m3u(client: tvp::Client, host: &str) -> anyhow::Result<()> {
    let mut playable = Vec::new();
    for item in client.lives().await? {
        if item.payable {
            tracing::info!(channel = item.id, title = item.title, "skipped: paid");
            continue;
        }
        match client.resolve(item.id).await {
            Ok(_) => playable.push(item),
            Err(e) => {
                tracing::info!(channel = item.id, title = item.title, error = %e, "skipped")
            }
        }
    }
    print!("{}", m3u::render(host, &playable));
    Ok(())
}
