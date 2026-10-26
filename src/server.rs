//! HTTP server: `GET /tvp/<id>.ts` streams the channel as MPEG-TS.

use crate::connection::Connection;
use crate::hls;
use crate::metrics::{
    ChannelMetrics, EndReason, Metrics, ProcessGuard, RequestOutcome, ResolvePhase, StreamGuard,
};
use crate::transport::Tracker;
use crate::tvp::{Client, ResolveError};
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempPath;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

type Tx = mpsc::Sender<Bytes>;
const CHUNK: usize = 64 * 1024;
const QUEUE: usize = 32;

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
    /// One permit per running stream, taken once its channel has resolved;
    /// requests that find none get 503.
    pub streams: Arc<Semaphore>,
    /// Cancel to end all running streams (on server shutdown).
    pub shutdown: CancellationToken,
    /// After `shutdown`, how long [`serve`] waits for responses to finish.
    pub shutdown_grace: Duration,
    /// Maximum time a client may leave output queued without accepting it.
    pub downstream_timeout: Duration,
    /// Overall resolve/startup ceiling, including waiting for both tracks.
    pub startup_timeout: Duration,
    /// Maximum silence of either required track after streaming starts.
    pub media_timeout: Duration,
}

/// Serve until `state.shutdown` is cancelled, then give running responses up
/// to `state.shutdown_grace` to finish: a client that stopped reading would
/// otherwise keep its response, and with it the server, alive forever.
pub async fn serve(listener: TcpListener, state: AppState) -> std::io::Result<()> {
    let shutdown = state.shutdown.clone();
    let grace = state.shutdown_grace;
    let server = axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<Connection>(),
    )
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
        .route("/tvp/{file}", get(stream).head(head))
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

async fn head(Path(file): Path<String>) -> Response {
    if channel_id(&file).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Unknown size prevents a false Content-Length:0 for the endless GET.
    struct Metadata;
    impl futures_core::Stream for Metadata {
        type Item = std::io::Result<Bytes>;
        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Ready(None)
        }
    }
    (
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(Metadata),
    )
        .into_response()
}

async fn stream(
    State(state): State<Arc<AppState>>,
    ConnectInfo(connection): ConnectInfo<Connection>,
    Path(file): Path<String>,
) -> Response {
    tokio::select! {
        biased;
        () = state.shutdown.cancelled() => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        result = tokio::time::timeout(state.startup_timeout,open_stream(state.clone(), connection, file)) => match result {
            Ok(response) => response,
            Err(_) => (StatusCode::GATEWAY_TIMEOUT,"upstream startup deadline\n").into_response(),
        }
    }
}

async fn open_stream(state: Arc<AppState>, connection: Connection, file: String) -> Response {
    let requested = Instant::now();
    let Some(channel) = channel_id(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if connection.0.is_none() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
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

    let (tx, rx) = mpsc::channel(QUEUE);
    let (ready, started) = oneshot::channel();
    let cancel = CancelOnDrop(state.shutdown.child_token());
    let session = metrics.stream(permit);
    let task = Session {
        state,
        channel,
        _master: master,
        child,
        session,
        connection,
        cancel: cancel.0.clone(),
    };
    tokio::spawn(task.run(tx, ready));
    if !matches!(started.await, Ok(Ok(()))) {
        request.finish(RequestOutcome::FfmpegError);
        return (
            StatusCode::BAD_GATEWAY,
            "upstream has no usable video/audio\n",
        )
            .into_response();
    }
    request.finish(RequestOutcome::Started);
    tracing::info!(channel, bitrate = selection.bitrate, "stream started");

    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(LiveBody::new(
            rx,
            metrics,
            requested,
            Some(cancel),
        )))
        .expect("valid response")
}

/// One admitted connection owns one child and a private master until cleanup.
/// No second process may append a reset timeline to this response.
struct Session {
    state: Arc<AppState>,
    channel: u64,
    _master: TempPath,
    child: TrackedChild,
    session: StreamGuard,
    connection: Connection,
    cancel: CancellationToken,
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Session {
    async fn run(mut self, tx: Tx, ready: oneshot::Sender<Result<(), ()>>) {
        let channel = self.channel;
        let state = &self.state;
        let mut stdout = self.child.child.stdout.take().expect("piped stdout");
        let mut tracker = Tracker::default();
        let started = tokio::select! {
            r = bootstrap(&mut stdout, &mut tracker) => r,
            () = self.cancel.cancelled() => Err(std::io::Error::other("startup cancelled")),
        };
        let mut result = match started {
            Ok(prefix) => {
                if ready.send(Ok(())).is_err() {
                    Ok(())
                } else {
                    tokio::select! {
                        r = forward(&mut stdout, &tx, state, &mut tracker, prefix) => r,
                        () = self.cancel.cancelled() => if state.shutdown.is_cancelled() { Err(EndReason::Shutdown) } else { Ok(()) },
                    }
                }
            }
            Err(_) => {
                let _ = ready.send(Err(()));
                Err(EndReason::UpstreamExit)
            }
        };
        // Own the child outside the cancellable forwarding future. Reap it before
        // the process gauge and stream permit can be released.
        if let Err(error) = self.child.child.kill().await {
            tracing::error!(channel, %error, "child cleanup failed");
        }
        drop(self.child);
        if state.shutdown.is_cancelled() {
            result = Err(EndReason::Shutdown);
        }
        if matches!(
            result,
            Err(EndReason::DownstreamTimeout | EndReason::MediaTimeout | EndReason::Shutdown)
        ) {
            if let Err(error) = self.connection.abort() {
                tracing::error!(channel, %error, "connection abort failed");
            }
        } else if result.is_err() {
            // dropping `tx` fails the response (see `LiveBody`), and hyper discards
            // response data it has not written out yet when the body fails, so let
            // it take everything sent so far first. Not on shutdown: a client that
            // stopped reading would hold us up forever.
            let drained = tokio::select! {
                r = tokio::time::timeout(state.downstream_timeout, tx.reserve_many(tx.max_capacity())) => r.is_ok(),
                () = self.cancel.cancelled() => true,
            };
            if !drained || state.shutdown.is_cancelled() {
                result = Err(EndReason::DownstreamTimeout);
                if let Err(error) = self.connection.abort() {
                    tracing::error!(channel, %error, "connection abort failed");
                }
            }
        }
        tracing::info!(
            channel,
            reason = result.err().map(EndReason::message),
            "stream ended"
        );
        self.session
            .finish(result.err().unwrap_or(EndReason::ClientDisconnect));
    }
}

/// Keep the process gauge tied to Child ownership, including cancellation of
/// a pending read or the entire session. Child still kills itself on drop.
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
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-sn",
            "-dn",
            "-c",
            "copy",
            "-streamid",
            "0:256",
            "-streamid",
            "1:257",
            "-f",
            "mpegts",
            "pipe:1",
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

/// Bounded startup chunks and the last payload activity of both required tracks.
struct Bootstrap {
    chunks: VecDeque<Bytes>,
    video: tokio::time::Instant,
    audio: tokio::time::Instant,
}

async fn bootstrap(
    stdout: &mut tokio::process::ChildStdout,
    tracker: &mut Tracker,
) -> std::io::Result<Bootstrap> {
    let mut chunks = VecDeque::new();
    let mut size = 0;
    let (mut video, mut audio) = (None, None);
    let mut buf = [0u8; CHUNK];
    loop {
        if size == CHUNK * QUEUE {
            return Err(std::io::Error::other("A/V bootstrap limit"));
        }
        let room = CHUNK.min(CHUNK * QUEUE - size);
        let n = stdout.read(&mut buf[..room]).await?;
        if n == 0 {
            return Err(std::io::Error::other("missing A/V output"));
        }
        let seen = tracker.feed(&buf[..n])?;
        let now = tokio::time::Instant::now();
        if seen.video {
            video = Some(now);
        }
        if seen.audio {
            audio = Some(now);
        }
        size += n;
        chunks.push_back(Bytes::copy_from_slice(&buf[..n]));
        if let (Some(video), Some(audio)) = (video, audio) {
            return Ok(Bootstrap {
                chunks,
                video,
                audio,
            });
        }
    }
}

async fn forward(
    stdout: &mut tokio::process::ChildStdout,
    tx: &Tx,
    state: &AppState,
    tracker: &mut Tracker,
    mut prefix: Bootstrap,
) -> Result<(), EndReason> {
    while let Some(chunk) = prefix.chunks.pop_front() {
        let (deadline, expired) = output_deadline(state, &prefix)?;
        match tokio::time::timeout_at(deadline, tx.send(chunk)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Ok(()),
            Err(_) => return Err(expired),
        }
    }
    let mut buf = [0u8; CHUNK];
    loop {
        // Reserve before reading: a full media queue must not also retain an
        // additional allocated chunk outside its budget.
        let (deadline, expired) = output_deadline(state, &prefix)?;
        let slot = match tokio::time::timeout_at(deadline, tx.reserve()).await {
            Ok(Ok(slot)) => slot,
            Ok(Err(_)) => return Ok(()),
            Err(_) => return Err(expired),
        };
        let deadline = prefix.video.min(prefix.audio) + state.media_timeout;
        if tokio::time::Instant::now() >= deadline {
            return Err(EndReason::MediaTimeout);
        }
        let n = match tokio::time::timeout_at(deadline, stdout.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => n,
            Ok(_) => return Err(EndReason::UpstreamExit),
            Err(_) => return Err(EndReason::MediaTimeout),
        };
        let seen = tracker
            .feed(&buf[..n])
            .map_err(|_| EndReason::UpstreamExit)?;
        let now = tokio::time::Instant::now();
        if seen.video {
            prefix.video = now;
        }
        if seen.audio {
            prefix.audio = now;
        }
        slot.send(Bytes::copy_from_slice(&buf[..n]));
    }
}

fn output_deadline(
    state: &AppState,
    activity: &Bootstrap,
) -> Result<(tokio::time::Instant, EndReason), EndReason> {
    let now = tokio::time::Instant::now();
    let media = activity.video.min(activity.audio) + state.media_timeout;
    if now >= media {
        return Err(EndReason::MediaTimeout);
    }
    let downstream = now + state.downstream_timeout;
    if media <= downstream {
        Ok((media, EndReason::MediaTimeout))
    } else {
        Ok((downstream, EndReason::DownstreamTimeout))
    }
}

/// The response body: the data `pump` sends, then an error once `pump` drops
/// its sender. A live stream never ends normally: upstream exit, track silence
/// or shutdown must be distinguishable from a successful finite programme.
struct LiveBody {
    rx: mpsc::Receiver<Bytes>,
    metrics: Arc<ChannelMetrics>,
    requested: Option<Instant>,
    _cancel: Option<CancelOnDrop>,
}

impl LiveBody {
    fn new(
        rx: mpsc::Receiver<Bytes>,
        metrics: Arc<ChannelMetrics>,
        requested: Instant,
        cancel: Option<CancelOnDrop>,
    ) -> Self {
        Self {
            rx,
            metrics,
            requested: Some(requested),
            _cancel: cancel,
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
        let mut body = LiveBody::new(rx, metrics.channel(1), Instant::now(), None);
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
