//! HTTP server: `GET /tvp/<id>.ts` streams the channel as MPEG-TS.

use crate::hls;
use crate::tvp::{Client, ResolveError};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_stream_shim::ReceiverStream;

/// Give up after this many consecutive quick failures.
const MAX_QUICK_FAILURES: u32 = 3;

#[derive(Clone)]
pub struct AppState {
    pub client: Client,
    pub ffmpeg: PathBuf,
    pub work_dir: PathBuf,
    pub retry_delay: Duration,
    /// ffmpeg runs shorter than this count as a quick failure.
    pub quick_failure: Duration,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/tvp/{file}", get(stream))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(Arc::new(state))
}

fn channel_id(file: &str) -> Option<u64> {
    file.strip_suffix(".ts")?.parse().ok()
}

async fn stream(State(state): State<Arc<AppState>>, Path(file): Path<String>) -> Response {
    let Some(channel) = channel_id(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // resolve once up front so errors become proper HTTP status codes
    let selection = match state.client.resolve(channel).await {
        Ok(sel) => sel,
        Err(ResolveError::Drm) => {
            tracing::warn!(channel, "DRM-protected, not supported");
            return (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "DRM-protected channel\n",
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(channel, error = %e, "resolve failed");
            return (StatusCode::BAD_GATEWAY, format!("{e}\n")).into_response();
        }
    };
    tracing::info!(channel, bitrate = selection.bitrate, "stream started");

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    tokio::spawn(async move {
        pump(state, channel, selection, tx).await;
        tracing::info!(channel, "stream ended");
    });

    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp2t")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("valid response")
}

/// Run ffmpeg, forward its output, and restart with a fresh URL when it stops.
/// Ends when the client goes away or ffmpeg keeps failing quickly.
async fn pump(
    state: Arc<AppState>,
    channel: u64,
    mut selection: hls::Selection,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
    // Private to this stream, so concurrent streams of one channel never
    // rewrite or delete the playlist another one's ffmpeg is reading. Random
    // name, created with O_EXCL and mode 0600; removed when `master` drops.
    let master = match tempfile::Builder::new()
        .prefix(&format!("tvp-{channel}-{}-", std::process::id()))
        .suffix(".m3u8")
        .tempfile_in(&state.work_dir)
    {
        Ok(file) => file.into_temp_path(),
        Err(e) => {
            tracing::error!(channel, error = %e, "cannot create master playlist");
            return;
        }
    };
    let mut failures = 0;
    loop {
        if let Err(e) = tokio::fs::write(&master, hls::single_variant_master(&selection)).await {
            tracing::error!(channel, error = %e, "cannot write master playlist");
            break;
        }
        let started = Instant::now();
        let client_gone = match run_ffmpeg(&state.ffmpeg, &master, &tx).await {
            Ok(gone) => gone,
            Err(e) => {
                tracing::error!(channel, error = %e, "cannot start ffmpeg");
                break;
            }
        };
        if client_gone {
            break;
        }
        let ran = started.elapsed();
        failures = if ran < state.quick_failure {
            failures + 1
        } else {
            0
        };
        if failures >= MAX_QUICK_FAILURES {
            tracing::error!(channel, failures, "giving up after quick failures");
            break;
        }
        tracing::warn!(channel, secs = ran.as_secs(), "ffmpeg ended, re-resolving");
        let resolved = tokio::select! {
            r = async {
                tokio::time::sleep(state.retry_delay).await;
                state.client.resolve(channel).await
            } => r,
            // don't restart ffmpeg for a client that left in the meantime
            () = tx.closed() => break,
        };
        match resolved {
            Ok(sel) => selection = sel,
            Err(e) => tracing::warn!(channel, error = %e, "re-resolve failed, retrying old URL"),
        }
    }
}

/// Returns Ok(true) if the client disconnected, Ok(false) if ffmpeg exited.
async fn run_ffmpeg(
    ffmpeg: &std::path::Path,
    master: &std::path::Path,
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> std::io::Result<bool> {
    let mut child = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error"])
        .args(["-protocol_whitelist", "file,http,https,tcp,tls,crypto,data"])
        .args(["-analyzeduration", "15000000", "-probesize", "20000000"])
        // fail upstream reads that stall for 15 s instead of hanging forever
        .args(["-rw_timeout", "15000000"])
        .arg("-i")
        .arg(master)
        .args([
            "-map", "0:v:0", "-map", "0:a:0?", "-c", "copy", "-f", "mpegts", "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf = vec![0u8; 64 * 1024];
    let gone = loop {
        let n = tokio::select! {
            r = stdout.read(&mut buf) => match r {
                Ok(0) | Err(_) => break false,
                Ok(n) => n,
            },
            // notice a disconnect even while ffmpeg produces no output
            () = tx.closed() => break true,
        };
        if tx
            .send(Ok(Bytes::copy_from_slice(&buf[..n])))
            .await
            .is_err()
        {
            break true;
        }
    };
    let _ = child.kill().await;
    Ok(gone)
}

/// Minimal adapter from an mpsc receiver to a `Stream`, avoiding an extra crate.
mod tokio_stream_shim {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::sync::mpsc::Receiver;

    pub struct ReceiverStream<T>(Receiver<T>);

    impl<T> ReceiverStream<T> {
        pub fn new(rx: Receiver<T>) -> Self {
            Self(rx)
        }
    }

    impl<T> futures_core::Stream for ReceiverStream<T> {
        type Item = T;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
            self.0.poll_recv(cx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::channel_id;

    #[test]
    fn parses_channel_ids() {
        assert_eq!(channel_id("399700.ts"), Some(399700));
        assert_eq!(channel_id("399700.m3u8"), None);
        assert_eq!(channel_id("abc.ts"), None);
        assert_eq!(channel_id("../1.ts"), None);
    }
}
