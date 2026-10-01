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
use tokio::process::{Child, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_stream_shim::ReceiverStream;
use tokio_util::sync::CancellationToken;

/// ffmpeg runs shorter than this count as a quick failure.
const QUICK: Duration = Duration::from_secs(30);
/// Give up after this many consecutive quick failures.
const MAX_QUICK_FAILURES: u32 = 3;

type Tx = mpsc::Sender<Result<Bytes, std::io::Error>>;

#[derive(Clone)]
pub struct AppState {
    pub client: Client,
    pub ffmpeg: PathBuf,
    pub work_dir: PathBuf,
    pub retry_delay: Duration,
    /// One permit per running stream; requests that find none get 503.
    pub streams: Arc<Semaphore>,
    /// Cancel to end all running streams (on server shutdown).
    pub shutdown: CancellationToken,
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
    let Ok(permit) = state.streams.clone().try_acquire_owned() else {
        tracing::warn!(channel, "too many streams");
        return (StatusCode::SERVICE_UNAVAILABLE, "too many streams\n").into_response();
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
    // likewise start ffmpeg before the 200 header is sent
    let master = state
        .work_dir
        .join(format!("tvp-{channel}-{}.m3u8", std::process::id()));
    let child = match start_ffmpeg(&state.ffmpeg, &master, &selection).await {
        Ok(child) => child,
        Err(e) => {
            tracing::error!(channel, error = %e, "cannot start ffmpeg");
            let _ = tokio::fs::remove_file(&master).await;
            return (StatusCode::INTERNAL_SERVER_ERROR, "cannot start ffmpeg\n").into_response();
        }
    };
    tracing::info!(channel, bitrate = selection.bitrate, "stream started");

    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(pump(state, channel, selection, master, child, tx, permit));

    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp2t")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("valid response")
}

/// Stream until the client goes away, ffmpeg keeps failing, or the server
/// shuts down. In the last two cases the response ends with an error, so the
/// client sees an aborted transfer rather than a normal end of stream.
async fn pump(
    state: Arc<AppState>,
    channel: u64,
    selection: hls::Selection,
    master: PathBuf,
    child: Child,
    tx: Tx,
    _permit: OwnedSemaphorePermit,
) {
    let result = tokio::select! {
        r = restart_loop(&state, channel, selection, &master, child, &tx) => r,
        () = state.shutdown.cancelled() => Err("server shutting down"),
    };
    let _ = tokio::fs::remove_file(&master).await;
    if let Err(reason) = result {
        let _ = tx.send(Err(std::io::Error::other(reason))).await;
    }
    tracing::info!(channel, "stream ended");
}

/// Forward ffmpeg's output, and restart it with a fresh URL when it stops.
/// Returns Ok when the client goes away, Err when ffmpeg keeps failing quickly
/// or cannot be restarted.
async fn restart_loop(
    state: &AppState,
    channel: u64,
    mut selection: hls::Selection,
    master: &std::path::Path,
    mut child: Child,
    tx: &Tx,
) -> Result<(), &'static str> {
    let mut failures = 0;
    loop {
        let started = Instant::now();
        if forward(child, tx).await {
            return Ok(());
        }
        let ran = started.elapsed();
        failures = if ran < QUICK { failures + 1 } else { 0 };
        if failures >= MAX_QUICK_FAILURES {
            tracing::error!(channel, failures, "giving up after quick failures");
            return Err("ffmpeg keeps failing");
        }
        tracing::warn!(channel, secs = ran.as_secs(), "ffmpeg ended, re-resolving");
        tokio::time::sleep(state.retry_delay).await;
        match state.client.resolve(channel).await {
            Ok(sel) => selection = sel,
            Err(e) => tracing::warn!(channel, error = %e, "re-resolve failed, retrying old URL"),
        }
        child = match start_ffmpeg(&state.ffmpeg, master, &selection).await {
            Ok(child) => child,
            Err(e) => {
                tracing::error!(channel, error = %e, "cannot start ffmpeg");
                return Err("cannot restart ffmpeg");
            }
        };
    }
}

/// Write the single-variant master for `selection` and start ffmpeg on it.
async fn start_ffmpeg(
    ffmpeg: &std::path::Path,
    master: &std::path::Path,
    selection: &hls::Selection,
) -> std::io::Result<Child> {
    let playlist = hls::single_variant_master(selection);
    if let Err(e) = tokio::fs::write(master, playlist).await {
        let msg = format!("cannot write {}: {e}", master.display());
        return Err(std::io::Error::new(e.kind(), msg));
    }
    Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error"])
        .args(["-protocol_whitelist", "file,http,https,tcp,tls,crypto"])
        .args(["-analyzeduration", "15000000", "-probesize", "20000000"])
        .arg("-i")
        .arg(master)
        .args([
            "-map", "0:v:0", "-map", "0:a:0?", "-c", "copy", "-f", "mpegts", "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

/// Forward ffmpeg's output to the client, then kill ffmpeg.
/// Returns true if the client disconnected, false if ffmpeg exited.
async fn forward(mut child: Child, tx: &Tx) -> bool {
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf = vec![0u8; 64 * 1024];
    let gone = loop {
        let n = match stdout.read(&mut buf).await {
            Ok(0) | Err(_) => break false,
            Ok(n) => n,
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
    gone
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
