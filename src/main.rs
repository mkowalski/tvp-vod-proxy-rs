use anyhow::Context;
use std::path::PathBuf;
use std::time::Duration;
use tvp_vod_proxy::{m3u, server, tvp};
use url::Url;

const USAGE: &str = "\
usage:
  tvp-vod-proxy [serve]             start the proxy (default)
  tvp-vod-proxy make-m3u HOST:PORT  print an M3U of all playable channels
  tvp-vod-proxy -h | --help         show this help

environment:
  PORT         listen port (default 8080)
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
    let port: u16 = env("PORT", 8080)?;
    let state = server::AppState {
        client,
        ffmpeg: PathBuf::from(env("FFMPEG", "ffmpeg".to_string())?),
        work_dir: std::env::temp_dir(),
        retry_delay: Duration::from_secs(1),
    };
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "listening");
    axum::serve(listener, server::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
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
