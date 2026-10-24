//! HTTP server: `GET /tvp/<id>.ts` streams the channel as MPEG-TS.

use crate::hls;
use crate::metrics::{
    ChannelMetrics, EndReason, Metrics, ProcessGuard, RequestOutcome, ResolvePhase, StreamGuard,
};
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
use tempfile::TempPath;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

/// Give up after this many consecutive quick failures.
const MAX_QUICK_FAILURES: u32 = 3;

type Tx = mpsc::Sender<Bytes>;

/// Shared configuration for all streams.
#[derive(Clone)]
pub struct AppState {
    pub client: Client,
    /// Process-local statistics shared by the streams and metrics endpoint.
    pub metrics: Arc<Metrics>,
    /// ffmpeg binary.
    pub ffmpeg: PathBuf,
    /// Directory for the per-stream master playlists given to ffmpeg.
    pub work_dir: PathBuf,
    /// Pause before re-resolving after ffmpeg exits.
    pub retry_delay: Duration,
    /// ffmpeg runs shorter than this count as a quick failure.
    pub quick_failure: Duration,
    /// One permit per running stream, taken once its channel has resolved;
    /// requests that find none get 503.
    pub streams: Arc<Semaphore>,
    /// Cancel to end all running streams (on server shutdown).
    pub shutdown: CancellationToken,
    /// After `shutdown`, how long [`serve`] waits for responses to finish.
    pub shutdown_grace: Duration,
}

/// Serve until `state.shutdown` is cancelled, then give running responses up
/// to `state.shutdown_grace` to finish: a client that stopped reading would
/// otherwise keep its response, and with it the server, alive forever.
pub async fn serve(listener: TcpListener, state: AppState) -> std::io::Result<()> {
    let shutdown = state.shutdown.clone();
    let grace = state.shutdown_grace;
    let server = axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned());
    tokio::select! {
        r = server => r,
        () = async {
            shutdown.cancelled().await;
            tokio::time::sleep(grace).await;
        } => {
            tracing::warn!("streams did not end in time, exiting");
            Ok(())
        }
    }
}

/// Routes: `GET /tvp/<id>.ts`, `GET /healthz` and `GET /metrics`.
pub fn router(state: AppState) -> Router {
    let metrics = state.metrics.clone();
    let streams = state.streams.clone();
    let capacity = streams.available_permits();
    let limit = if capacity == Semaphore::MAX_PERMITS {
        0
    } else {
        capacity
    };
    Router::new()
        .route("/tvp/{file}", get(stream))
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/metrics",
            get(move || {
                let text =
                    metrics.render(limit, capacity.saturating_sub(streams.available_permits()));
                async move {
                    (
                        [
                            (
                                header::CONTENT_TYPE,
                                "text/plain; version=0.0.4; charset=utf-8",
                            ),
                            (header::CACHE_CONTROL, "no-store"),
                        ],
                        text,
                    )
                }
            }),
        )
        .with_state(Arc::new(state))
}

fn channel_id(file: &str) -> Option<u64> {
    file.strip_suffix(".ts")?.parse().ok()
}

async fn resolve(
    state: &AppState,
    channel: u64,
    phase: ResolvePhase,
) -> Result<hls::Selection, ResolveError> {
    let timer = state.metrics.resolve(phase);
    let result = state.client.resolve(channel).await;
    timer.finish(&result);
    result
}

async fn stream(State(state): State<Arc<AppState>>, Path(file): Path<String>) -> Response {
    let requested = Instant::now();
    let Some(channel) = channel_id(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let request = state.metrics.request();
    // resolve once up front so errors become proper HTTP status codes
    let selection = match resolve(&state, channel, ResolvePhase::Initial).await {
        Ok(sel) => sel,
        Err(e @ ResolveError::Drm) => {
            tracing::warn!(channel, "DRM-protected, not supported");
            request.finish(RequestOutcome::Drm);
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, format!("{e}\n")).into_response();
        }
        Err(e) => {
            // the error may contain the signed CDN URL: log it, don't return it
            tracing::error!(channel, error = %e, "resolve failed");
            request.finish(RequestOutcome::ResolveError);
            return (StatusCode::BAD_GATEWAY, "upstream error\n").into_response();
        }
    };
    // take a slot only once resolved, so slow resolves can't lock everyone out
    let Ok(permit) = state.streams.clone().try_acquire_owned() else {
        tracing::warn!(channel, "too many streams");
        request.finish(RequestOutcome::Capacity);
        return (StatusCode::SERVICE_UNAVAILABLE, "too many streams\n").into_response();
    };
    // as with resolving, start ffmpeg before the 200 header is sent. The master
    // playlist is private to this stream, so concurrent streams of one channel
    // never rewrite or delete the playlist another one's ffmpeg is reading.
    // Random name, created with O_EXCL and mode 0600; removed when `master` drops.
    let master = match tempfile::Builder::new()
        .prefix(&format!("tvp-{channel}-{}-", std::process::id()))
        .suffix(".m3u8")
        .tempfile_in(&state.work_dir)
    {
        Ok(file) => file.into_temp_path(),
        Err(e) => {
            tracing::error!(channel, error = %e, "cannot create master playlist");
            request.finish(RequestOutcome::PlaylistError);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot create master playlist\n",
            )
                .into_response();
        }
    };
    // Only resolved, admitted channels consume per-channel metric series.
    let metrics = state.metrics.channel(channel);
    let child = match start_ffmpeg(&state.ffmpeg, &master, &selection, &metrics).await {
        Ok(child) => child,
        Err(e) => {
            tracing::error!(channel, error = %e, "cannot start ffmpeg");
            request.finish(RequestOutcome::FfmpegError);
            return (StatusCode::INTERNAL_SERVER_ERROR, "cannot start ffmpeg\n").into_response();
        }
    };
    tracing::info!(channel, bitrate = selection.bitrate, "stream started");

    let (tx, rx) = mpsc::channel(32);
    let session = metrics.stream(permit);
    request.finish(RequestOutcome::Started);
    tokio::spawn(pump(state, channel, selection, master, child, tx, session));

    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp2t")
        .body(Body::from_stream(LiveBody::new(rx, metrics, requested)))
        .expect("valid response")
}

/// Stream until the client goes away, ffmpeg keeps failing or can't be
/// restarted, or the server shuts down. In all but the first case the
/// response ends with an error, so the client sees an aborted transfer rather
/// than a normal end of stream.
async fn pump(
    state: Arc<AppState>,
    channel: u64,
    selection: hls::Selection,
    master: TempPath,
    child: TrackedChild,
    tx: Tx,
    session: StreamGuard,
) {
    let result = tokio::select! {
        r = restart_loop(&state, channel, selection, &master, child, &tx, session.metrics()) => r,
        () = state.shutdown.cancelled() => Err(EndReason::Shutdown),
    };
    if result.is_err() {
        // dropping `tx` fails the response (see `LiveBody`), and hyper discards
        // response data it has not written out yet when the body fails, so let
        // it take everything sent so far first. Not on shutdown: a client that
        // stopped reading would hold us up forever.
        tokio::select! {
            _ = tx.reserve_many(tx.max_capacity()) => {}
            () = state.shutdown.cancelled() => {}
        }
    }
    tracing::info!(
        channel,
        reason = result.err().map(EndReason::message),
        "stream ended"
    );
    session.finish(result.err().unwrap_or(EndReason::ClientDisconnect));
}

/// Forward ffmpeg's output, and restart it with a fresh URL when it stops.
/// Returns Ok when the client goes away, Err when ffmpeg keeps failing quickly
/// or cannot be restarted.
async fn restart_loop(
    state: &AppState,
    channel: u64,
    mut selection: hls::Selection,
    master: &std::path::Path,
    mut child: TrackedChild,
    tx: &Tx,
    metrics: &Arc<ChannelMetrics>,
) -> Result<(), EndReason> {
    let mut failures = 0;
    loop {
        let started = Instant::now();
        if forward(child, tx).await {
            return Ok(());
        }
        let ran = started.elapsed();
        failures = if ran < state.quick_failure {
            failures + 1
        } else {
            0
        };
        if failures >= MAX_QUICK_FAILURES {
            tracing::error!(channel, failures, "giving up after quick failures");
            return Err(EndReason::QuickFailures);
        }
        tracing::warn!(channel, secs = ran.as_secs(), "ffmpeg ended, re-resolving");
        let resolved = tokio::select! {
            r = async {
                tokio::time::sleep(state.retry_delay).await;
                resolve(state, channel, ResolvePhase::Retry).await
            } => r,
            // don't restart ffmpeg for a client that left in the meantime
            () = tx.closed() => return Ok(()),
        };
        match resolved {
            Ok(sel) => selection = sel,
            Err(e) => tracing::warn!(channel, error = %e, "re-resolve failed, retrying old URL"),
        }
        child = match start_ffmpeg(&state.ffmpeg, master, &selection, metrics).await {
            Ok(child) => {
                metrics.restarted();
                child
            }
            Err(e) => {
                tracing::error!(channel, error = %e, "cannot start ffmpeg");
                return Err(EndReason::RestartError);
            }
        };
    }
}

/// Keep the process gauge tied to Child ownership, including cancellation of
/// a pending read or the entire restart loop. Child still kills itself on drop.
struct TrackedChild {
    child: Child,
    _metrics: ProcessGuard,
}

/// Write the single-variant master for `selection` and start ffmpeg on it.
async fn start_ffmpeg(
    ffmpeg: &std::path::Path,
    master: &std::path::Path,
    selection: &hls::Selection,
    metrics: &Arc<ChannelMetrics>,
) -> std::io::Result<TrackedChild> {
    let playlist = hls::single_variant_master(selection);
    if let Err(e) = tokio::fs::write(master, playlist).await {
        let msg = format!("cannot write {}: {e}", master.display());
        return Err(std::io::Error::new(e.kind(), msg));
    }
    let child = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error"])
        .args(["-protocol_whitelist", "file,http,https,tcp,tls,crypto"])
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
    Ok(TrackedChild {
        child,
        _metrics: metrics.process(),
    })
}

/// Forward ffmpeg's output to the client, then kill ffmpeg.
/// Returns true if the client disconnected, false if ffmpeg exited.
async fn forward(mut child: TrackedChild, tx: &Tx) -> bool {
    let mut stdout = child.child.stdout.take().expect("piped stdout");
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
        if tx.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() {
            break true;
        }
    };
    let _ = child.child.kill().await;
    gone
}

/// The response body: the data `pump` sends, then an error once `pump` drops
/// its sender. A live stream never ends normally: with the client still there,
/// `pump` only stops when ffmpeg keeps failing or can't be restarted, or on
/// shutdown, and the client must see an aborted transfer, not an end of
/// programme.
struct LiveBody {
    rx: mpsc::Receiver<Bytes>,
    metrics: Arc<ChannelMetrics>,
    requested: Option<Instant>,
}

impl LiveBody {
    fn new(rx: mpsc::Receiver<Bytes>, metrics: Arc<ChannelMetrics>, requested: Instant) -> Self {
        Self {
            rx,
            metrics,
            requested: Some(requested),
        }
    }
}

impl futures_core::Stream for LiveBody {
    type Item = std::io::Result<Bytes>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx).map(|data| {
            Some(match data {
                Some(data) => {
                    if !data.is_empty() {
                        let first = self.requested.take().map(|started| started.elapsed());
                        self.metrics.output(data.len(), first);
                    }
                    Ok(data)
                }
                None => Err(std::io::Error::other("stream aborted")),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{LiveBody, channel_id};
    use crate::metrics::Metrics;
    use bytes::Bytes;
    use futures_core::Stream;
    use std::pin::Pin;
    use std::time::Instant;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn metrics_count_only_nonempty_output_handed_to_http() {
        let metrics = Metrics::default();
        let (tx, rx) = mpsc::channel(4);
        for data in [b"".as_slice(), b"sent", b"also", b"discarded"] {
            tx.send(Bytes::copy_from_slice(data)).await.unwrap();
        }
        let mut body = LiveBody::new(rx, metrics.channel(1), Instant::now());
        let empty = std::future::poll_fn(|cx| Pin::new(&mut body).poll_next(cx)).await;
        assert!(empty.unwrap().unwrap().is_empty());
        assert!(
            metrics
                .render(0, 0)
                .contains("tvp_stream_startup_seconds_count 0\n")
        );
        for expected in [b"sent", b"also"] {
            let chunk = std::future::poll_fn(|cx| Pin::new(&mut body).poll_next(cx)).await;
            assert_eq!(&chunk.unwrap().unwrap()[..], expected);
        }
        drop(body);
        let text = metrics.render(0, 0);
        assert!(text.contains("tvp_stream_bytes_total{channel=\"1\"} 8\n"));
        assert!(text.contains("tvp_stream_startup_seconds_count 1\n"));
    }

    #[test]
    fn parses_channel_ids() {
        assert_eq!(channel_id("399700.ts"), Some(399700));
        assert_eq!(channel_id("399700.m3u8"), None);
        assert_eq!(channel_id("abc.ts"), None);
        assert_eq!(channel_id("../1.ts"), None);
    }
}
