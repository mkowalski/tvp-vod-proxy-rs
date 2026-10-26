//! End-to-end tests against a mock TVP API/CDN. Streaming tests need ffmpeg and
//! ffprobe on PATH; if they are missing the tests are skipped with a message,
//! or fail when `CI` is set. Tests that run a shell script in place of ffmpeg
//! always run.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tvp_vod_proxy::{server, tvp};
use url::Url;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// True if ffmpeg and ffprobe are on PATH. On CI they must be, so a broken
/// install fails the streaming tests instead of silently skipping them.
fn ffmpeg_available() -> bool {
    if have("ffmpeg") && have("ffprobe") {
        return true;
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "ffmpeg/ffprobe not found (required when CI is set)"
    );
    eprintln!("skipping: ffmpeg/ffprobe not found");
    false
}

/// Mount every file in `dir` under `/cdn/<name>`.
async fn serve_dir(mock: &MockServer, dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        let body = std::fs::read(&p).unwrap();
        let name = p.file_name().unwrap().to_str().unwrap().to_string();
        Mock::given(method("GET"))
            .and(path(format!("/cdn/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(mock)
            .await;
    }
}

fn api_playlist(channel: u64, master_url: &str) -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/api/products/{channel}/videos/playlist")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sources": {"HLS": [{"src": master_url}]}
        })))
}

async fn api_returns(mock: &MockServer, channel: u64, master_url: &str) {
    api_playlist(channel, master_url).mount(mock).await;
}

/// Build a TVP-like VOD HLS: two fMP4 video variants and a separate audio rendition.
fn make_hls(dir: &Path) {
    let run = |args: &[&str]| {
        let st = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(st.success(), "ffmpeg {args:?}");
    };
    let hls = [
        "-f",
        "hls",
        "-hls_time",
        "2",
        "-hls_playlist_type",
        "vod",
        "-hls_segment_type",
        "fmp4",
    ];
    for (name, size) in [("v1", "320x180"), ("v2", "160x90")] {
        let mut a = vec![
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=6:rate=25",
            "-s",
            size,
            "-c:v",
            "libx264",
            "-g",
            "50",
            "-pix_fmt",
            "yuv420p",
            "-hls_fmp4_init_filename",
        ];
        let init = format!("{name}_init.mp4");
        let seg = format!("{name}_%d.m4s");
        let out = format!("{name}.m3u8");
        a.push(&init);
        a.extend(hls);
        a.extend(["-hls_segment_filename", &seg, &out]);
        run(&a);
    }
    run(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=6",
        "-c:a",
        "aac",
        "-ac",
        "2",
        "-hls_fmp4_init_filename",
        "a1_init.mp4",
        "-f",
        "hls",
        "-hls_time",
        "2",
        "-hls_playlist_type",
        "vod",
        "-hls_segment_type",
        "fmp4",
        "-hls_segment_filename",
        "a1_%d.m4s",
        "a1.m3u8",
    ]);
    std::fs::write(
        dir.join("master.m3u8"),
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",LANGUAGE=\"pol\",DEFAULT=YES,URI=\"a1.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=900000,AVERAGE-BANDWIDTH=600000,RESOLUTION=320x180,AUDIO=\"aud\"\n\
         v1.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=300000,AVERAGE-BANDWIDTH=200000,RESOLUTION=160x90,AUDIO=\"aud\"\n\
         v2.m3u8\n",
    )
    .unwrap();
}

fn proxy_state(api: &str, max_bitrate: u64) -> server::AppState {
    server::AppState {
        client: tvp::Client::new(Url::parse(api).unwrap(), max_bitrate).unwrap(),
        metrics: Arc::default(),
        ffmpeg: PathBuf::from("ffmpeg"),
        work_dir: std::env::temp_dir(),
        streams: Arc::new(Semaphore::new(Semaphore::MAX_PERMITS)),
        shutdown: CancellationToken::new(),
        // longer than any test waits: shutdown must complete without it
        shutdown_grace: Duration::from_secs(60),
        downstream_timeout: Duration::from_secs(30),
        startup_timeout: Duration::from_secs(120),
        media_timeout: Duration::from_secs(30),
    }
}

/// Serve on a free port, with graceful shutdown on `state.shutdown`.
async fn serve_proxy(state: server::AppState) -> (SocketAddr, JoinHandle<std::io::Result<()>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (addr, tokio::spawn(server::serve(listener, state)))
}

async fn start_proxy(api: &str, max_bitrate: u64) -> SocketAddr {
    serve_proxy(proxy_state(api, max_bitrate)).await.0
}

/// Read a response to its end. Also returns how it ended: `Err` if the
/// transfer was aborted rather than finished normally.
async fn read_to_end(resp: &mut reqwest::Response) -> (Vec<u8>, reqwest::Result<()>) {
    let mut body = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            Ok(None) => return (body, Ok(())),
            Err(e) => return (body, Err(e)),
        }
    }
}

/// Download a stream (bounded by time) and return ffprobe's stream summary and
/// the number of video packets (one per frame).
async fn probe(url: &str) -> (Vec<String>, u64) {
    let (bytes, _) = tokio::time::timeout(Duration::from_secs(60), async {
        let mut resp = reqwest::get(url).await.unwrap();
        // the response ends with an error once the proxy gives up on the VOD input
        read_to_end(&mut resp).await
    })
    .await
    .expect("stream finished");
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &bytes).unwrap();
    let decode = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-xerror", "-i"])
        .arg(tmp.path())
        .args(["-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        decode.status.success() && decode.stderr.is_empty(),
        "complete recording must decode: {}",
        String::from_utf8_lossy(&decode.stderr)
    );
    for track in ["v:0", "a:0"] {
        let clock = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                track,
                "-show_packets",
                "-show_entries",
                "packet=dts",
                "-of",
                "csv=p=0",
            ])
            .arg(tmp.path())
            .output()
            .unwrap();
        assert!(clock.status.success());
        let ticks: Vec<i64> = String::from_utf8(clock.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| line.split(',').next()?.parse().ok())
            .collect();
        assert!(ticks.len() > 100, "missing track packets");
        assert!(
            ticks.windows(2).all(|p| p[1] >= p[0]),
            "{track} contains a reset clock"
        );
    }
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,width,channels",
            "-of",
            "csv=p=0",
        ])
        .arg(tmp.path())
        .output()
        .unwrap();
    let mut lines: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.trim_end_matches(',').to_string())
        .filter(|l| !l.is_empty())
        .collect();
    // ffprobe lists streams both under the program and globally
    lines.sort();
    lines.dedup();
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_packets",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(tmp.path())
        .output()
        .unwrap();
    let frames = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .find_map(|l| l.parse().ok())
        .expect("video packet count");
    (lines, frames)
}

#[tokio::test]
async fn streams_one_video_and_one_audio_as_mpegts() {
    if !ffmpeg_available() {
        return;
    }
    let hls = tempfile::tempdir().unwrap();
    make_hls(hls.path());
    let mock = MockServer::start().await;
    serve_dir(&mock, hls.path()).await;
    api_returns(&mock, 1, &format!("{}/cdn/master.m3u8", mock.uri())).await;
    // One recording must contain one timeline, not replayed reset-clock passes.
    let one_pass = 6 * 25;

    // no limit: top variant
    let addr = start_proxy(&mock.uri(), 0).await;
    let (streams, frames) = probe(&format!("http://{addr}/tvp/1.ts")).await;
    assert_eq!(streams, vec!["audio,2", "video,320"], "{streams:?}");
    assert_eq!(frames, one_pass);

    // capped: lower variant
    let addr = start_proxy(&mock.uri(), 300_000).await;
    let (streams, frames) = probe(&format!("http://{addr}/tvp/1.ts")).await;
    assert_eq!(streams, vec!["audio,2", "video,160"], "{streams:?}");
    assert_eq!(frames, one_pass);
}

#[tokio::test]
async fn drm_channel_returns_415() {
    let mock = MockServer::start().await;
    Mock::given(path("/cdn/master.m3u8"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n"),
        )
        .mount(&mock)
        .await;
    Mock::given(path("/cdn/v.m3u8"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/media_drm.m3u8")),
        )
        .mount(&mock)
        .await;
    api_returns(&mock, 2, &format!("{}/cdn/master.m3u8", mock.uri())).await;
    let addr = start_proxy(&mock.uri(), 0).await;
    let r = reqwest::get(format!("http://{addr}/tvp/2.ts"))
        .await
        .unwrap();
    assert_eq!(r.status(), 415);
    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(&text, "tvp_stream_requests_total{outcome=\"drm\"}"),
        1.0
    );
    assert_eq!(
        metric_value(
            &text,
            "tvp_resolve_total{phase=\"initial\",outcome=\"drm\"}"
        ),
        1.0
    );
    assert_eq!(metric_value(&text, "tvp_stream_startup_seconds_count"), 0.0);
}

#[tokio::test]
async fn drm_in_master_or_audio_returns_415() {
    let mock = MockServer::start().await;
    let session_key = "#EXTM3U\n\
                       #EXT-X-SESSION-KEY:METHOD=SAMPLE-AES,URI=\"skd://key\"\n\
                       #EXT-X-STREAM-INF:BANDWIDTH=1\n\
                       v.m3u8\n";
    let drm_audio = "#EXTM3U\n\
                     #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",DEFAULT=YES,URI=\"a.m3u8\"\n\
                     #EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\n\
                     v.m3u8\n";
    for (name, body) in [
        ("session_key.m3u8", session_key),
        ("drm_audio.m3u8", drm_audio),
        ("a.m3u8", include_str!("fixtures/media_drm.m3u8")),
    ] {
        Mock::given(path(format!("/cdn/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&mock)
            .await;
    }
    // v.m3u8 is clear; mounted last, as the first matching mock wins
    serve_media(&mock).await;
    api_returns(&mock, 16, &format!("{}/cdn/session_key.m3u8", mock.uri())).await;
    api_returns(&mock, 17, &format!("{}/cdn/drm_audio.m3u8", mock.uri())).await;
    let addr = start_proxy(&mock.uri(), 0).await;
    for id in [16, 17] {
        let r = reqwest::get(format!("http://{addr}/tvp/{id}.ts"))
            .await
            .unwrap();
        assert_eq!(r.status(), 415, "channel {id}");
    }
}

#[tokio::test]
async fn api_error_returns_502() {
    let mock = MockServer::start().await;
    Mock::given(path("/api/products/3/videos/playlist"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string(r#"{"code":"GEOIP_FILTER_FAILED"}"#),
        )
        .mount(&mock)
        .await;
    let addr = start_proxy(&mock.uri(), 0).await;
    let r = reqwest::get(format!("http://{addr}/tvp/3.ts"))
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
    // the error text can carry signed upstream URLs: keep it out of the body
    let body = r.text().await.unwrap();
    assert!(!body.contains(&mock.uri()), "{body}");
    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(
            &text,
            "tvp_stream_requests_total{outcome=\"resolve_error\"}"
        ),
        1.0
    );
    assert_eq!(
        metric_value(
            &text,
            "tvp_resolve_total{phase=\"initial\",outcome=\"http_403\"}"
        ),
        1.0
    );
    assert!(!text.contains(&mock.uri()), "{text}");
}

#[tokio::test]
async fn master_without_playable_variant_returns_502() {
    let mock = MockServer::start().await;
    // no BANDWIDTH: not a media playlist, but no variant can be chosen either
    Mock::given(path("/cdn/master.m3u8"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1920x1080\nv.m3u8\n"),
        )
        .mount(&mock)
        .await;
    api_returns(&mock, 18, &format!("{}/cdn/master.m3u8", mock.uri())).await;
    let addr = start_proxy(&mock.uri(), 0).await;
    let r = reqwest::get(format!("http://{addr}/tvp/18.ts"))
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
}

#[tokio::test]
async fn unknown_paths_return_404() {
    let mock = MockServer::start().await;
    let addr = start_proxy(&mock.uri(), 0).await;
    for p in ["/tvp/abc.ts", "/tvp/1.m3u8", "/other"] {
        let r = reqwest::get(format!("http://{addr}{p}")).await.unwrap();
        assert_eq!(r.status(), 404, "{p}");
    }
    let r = reqwest::get(format!("http://{addr}/healthz"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn head_does_not_resolve_or_spawn() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 40, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg("echo run >> \"$0.runs\"; exec sleep 1000");
    let runs = ffmpeg.with_extension("runs");
    let state = server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    };
    let stop = state.shutdown.clone();
    let slots = state.streams.clone();
    let before = slots.available_permits();
    let (addr, serving) = serve_proxy(state).await;
    let r = reqwest::Client::new()
        .head(format!("http://{addr}/tvp/40.ts"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "video/mp2t");
    assert!(!r.headers().contains_key("content-length"));
    assert!(
        mock.received_requests().await.unwrap().is_empty(),
        "HEAD contacted upstream"
    );
    assert!(!runs.exists());
    assert_eq!(slots.available_permits(), before);
    assert_eq!(
        reqwest::Client::new()
            .head(format!("http://{addr}/tvp/bad.ts"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    stop.cancel();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn missing_tracks_cannot_start_a_successful_tv_stream() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 42, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    for script in [
        "exit 0",
        "printf '\\107\\101\\000\\020'; printf '%0184d' 0",
        "printf '\\107\\101\\001\\020'; printf '%0184d' 0",
    ] {
        let (_dir, ffmpeg) = stub_ffmpeg(script);
        let state = server::AppState {
            ffmpeg,
            ..proxy_state(&mock.uri(), 0)
        };
        let stop = state.shutdown.clone();
        let (addr, serving) = serve_proxy(state).await;
        let r = reqwest::get(format!("http://{addr}/tvp/42.ts"))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            502,
            "incomplete A/V input cannot commit HTTP200"
        );
        stop.cancel();
        serving.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn audio_activity_cannot_hide_a_stalled_video_track() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 43, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg(
        "printf '\\107\\101\\000\\020'; printf '%0184d' 0; while :; do printf '\\107\\101\\001\\020'; printf '%0184d' 0; sleep 0.01; done",
    );
    let state = server::AppState {
        ffmpeg,
        media_timeout: Duration::from_secs(1),
        ..proxy_state(&mock.uri(), 0)
    };
    let stop = state.shutdown.clone();
    let (addr, serving) = serve_proxy(state).await;
    let mut r = reqwest::get(format!("http://{addr}/tvp/43.ts"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let result = tokio::time::timeout(Duration::from_secs(33), read_to_end(&mut r)).await;
    assert!(result.is_ok(), "audio kept a dead video stream alive");
    assert!(result.unwrap().1.is_err());
    stop.cancel();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn video_activity_cannot_hide_a_stalled_audio_track() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 44, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg(
        "emit X; while :; do printf '\\107\\101\\000\\020'; printf '%0184d' 0; sleep 0.01; done",
    );
    let state = server::AppState {
        ffmpeg,
        media_timeout: Duration::from_secs(1),
        ..proxy_state(&mock.uri(), 0)
    };
    let stop = state.shutdown.clone();
    let metrics = state.metrics.clone();
    let (addr, serving) = serve_proxy(state).await;
    let mut r = reqwest::get(format!("http://{addr}/tvp/44.ts"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), read_to_end(&mut r))
            .await
            .unwrap()
            .1
            .is_err()
    );
    stop.cancel();
    serving.await.unwrap().unwrap();
    assert_eq!(
        metric_value(
            &metrics.render(0, 0),
            "tvp_stream_ends_total{reason=\"media_timeout\"}"
        ),
        1.0
    );
}

#[tokio::test]
async fn startup_accepts_a_large_video_prefix_before_audio() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 45, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg(
        "n=0; while [ $n -lt 512 ]; do printf '\\107\\101\\000\\020'; printf '%0184d' 0; n=$((n+1)); done; emit X; exec sleep 1000",
    );
    let state = server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    };
    let stop = state.shutdown.clone();
    let (addr, serving) = serve_proxy(state).await;
    let r = reqwest::get(format!("http://{addr}/tvp/45.ts"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    drop(r);
    stop.cancel();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn startup_deadline_and_disconnect_reap_before_releasing_capacity() {
    use tokio::io::AsyncWriteExt;
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 46, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    for disconnect in [false, true] {
        let (_dir, ffmpeg) = stub_ffmpeg("echo $$ > \"$0.pid\"; exec sleep 1000");
        let pid_file = ffmpeg.with_extension("pid");
        let work = tempfile::tempdir().unwrap();
        let slots = Arc::new(Semaphore::new(1));
        let state = server::AppState {
            ffmpeg,
            work_dir: work.path().into(),
            streams: slots.clone(),
            startup_timeout: Duration::from_secs(1),
            ..proxy_state(&mock.uri(), 0)
        };
        let stop = state.shutdown.clone();
        let (addr, serving) = serve_proxy(state).await;
        if disconnect {
            let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
            socket
                .write_all(b"GET /tvp/46.ts HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            wait_until("startup child PID", || pid_file.exists()).await;
            drop(socket);
        } else {
            assert_eq!(
                tokio::time::timeout(
                    Duration::from_secs(2),
                    reqwest::get(format!("http://{addr}/tvp/46.ts"))
                )
                .await
                .unwrap()
                .unwrap()
                .status(),
                504
            );
        }
        let pid = std::fs::read_to_string(pid_file).unwrap();
        wait_until("startup reap", || !running(pid.trim())).await;
        wait_until("startup capacity recovery", || {
            slots.available_permits() == 1
        })
        .await;
        wait_until("startup private-master cleanup", || {
            std::fs::read_dir(work.path()).unwrap().next().is_none()
        })
        .await;
        stop.cancel();
        serving.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn endless_one_track_bootstrap_is_rejected_with_bounded_memory() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 47, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) =
        stub_ffmpeg("while :; do printf '\\107\\101\\001\\020'; printf '%0184d' 0; done");
    let state = server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    };
    let stop = state.shutdown.clone();
    let (addr, serving) = serve_proxy(state).await;
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        reqwest::get(format!("http://{addr}/tvp/47.ts")),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(r.status(), 502);
    stop.cancel();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn bootstrap_forwarding_cannot_extend_the_media_silence_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 48, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg(
        "echo $$ > \"$0.pid\"; n=0; while [ $n -lt 10000 ]; do printf '\\107\\101\\000\\020'; printf '%0184d' 0; n=$((n+1)); done; emit X; exec sleep 1000",
    );
    let pid_file = ffmpeg.with_extension("pid");
    let state = server::AppState {
        ffmpeg,
        media_timeout: Duration::from_secs(1),
        downstream_timeout: Duration::from_secs(10),
        ..proxy_state(&mock.uri(), 0)
    };
    let stop = state.shutdown.clone();
    let listener = tokio::net::TcpSocket::new_v4().unwrap();
    listener.set_send_buffer_size(1024).unwrap();
    listener.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = listener.listen(128).unwrap();
    let addr = listener.local_addr().unwrap();
    let serving = tokio::spawn(server::serve(listener, state));
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(1024).unwrap();
    let mut socket = socket.connect(addr).await.unwrap();
    socket
        .write_all(b"GET /tvp/48.ts HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    socket.read_exact(&mut [0; 1]).await.unwrap();
    let pid = std::fs::read_to_string(pid_file).unwrap();
    let reaped = tokio::time::timeout(Duration::from_secs(3), async {
        while running(pid.trim()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    drop(socket);
    stop.cancel();
    serving.await.unwrap().unwrap();
    assert!(
        reaped.is_ok(),
        "bootstrap forwarding hid silent media beyond its deadline"
    );
}

#[tokio::test]
async fn shutdown_cancels_initial_resolution_before_spawning() {
    use tokio::io::AsyncWriteExt;
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    let resolving = Arc::new(AtomicBool::new(false));
    let started = resolving.clone();
    let source = format!("{}/cdn/live.m3u8", mock.uri());
    Mock::given(path("/api/products/49/videos/playlist"))
        .respond_with(move |_: &wiremock::Request| {
            started.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"sources":{"HLS":[{"src":source}]}}))
                .set_delay(Duration::from_secs(3))
        })
        .mount(&mock)
        .await;
    let (_dir, ffmpeg) = stub_ffmpeg("echo run >> \"$0.runs\"; emit X; exec sleep 1000");
    let runs = ffmpeg.with_extension("runs");
    let state = server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    };
    let stop = state.shutdown.clone();
    let metrics = state.metrics.clone();
    let (addr, mut serving) = serve_proxy(state).await;
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(b"GET /tvp/49.ts HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    wait_until("initial resolution", || resolving.load(Ordering::SeqCst)).await;
    stop.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(1), &mut serving).await;
    let clean = stopped.is_ok();
    drop(socket);
    if !clean {
        serving.await.unwrap().unwrap();
    }
    assert!(clean, "shutdown waited for initial upstream resolution");
    assert!(
        !runs.exists(),
        "shutdown allowed an unnecessary child start"
    );
    assert_eq!(
        metric_value(
            &metrics.render(0, 0),
            "tvp_resolves_in_flight{phase=\"initial\"}"
        ),
        0.0
    );
}

#[tokio::test]
async fn stalled_readers_are_reset_without_draining() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 41, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg("echo $$ > \"$0.pid\"; while :; do emit X; done");
    let pid_file = ffmpeg.with_extension("pid");
    let slots = Arc::new(Semaphore::new(1));
    let state = server::AppState {
        ffmpeg,
        streams: slots.clone(),
        downstream_timeout: Duration::from_secs(1),
        ..proxy_state(&mock.uri(), 0)
    };
    let stop = state.shutdown.clone();
    let (addr, serving) = serve_proxy(state).await;
    let mut retained = Vec::new();
    for _ in 0..2 {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(1024).unwrap();
        let mut socket = socket.connect(addr).await.unwrap();
        socket
            .write_all(b"GET /tvp/41.ts HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut first = [0; 1];
        socket.read_exact(&mut first).await.unwrap();
        wait_until("stub PID", || pid_file.exists()).await;
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(33), async {
            loop {
                if socket.take_error().unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "stalled HTTP body was retained without resetting its connection"
        );
        wait_until("stalled child cleanup", || !running(pid.trim())).await;
        wait_until("capacity recovery", || slots.available_permits() == 1).await;
        retained.push(socket);
    }
    drop(retained);
    stop.cancel();
    serving.await.unwrap().unwrap();
}

/// Count running ffmpeg processes whose command line mentions `needle`.
fn ffmpeg_running(needle: &str) -> usize {
    let out = Command::new("ps")
        .args(["-axo", "command"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("ffmpeg") && l.contains(needle))
        .count()
}

/// Poll `cond` until it holds. Panics after 5 s, naming `what` it waited for.
async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..50 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn client_disconnect_stops_ffmpeg() {
    if !ffmpeg_available() {
        return;
    }
    // A live playlist (no ENDLIST) that the mock keeps serving: ffmpeg would
    // run forever unless the proxy kills it.
    let hls = tempfile::tempdir().unwrap();
    make_hls(hls.path());
    for f in ["v1.m3u8", "v2.m3u8", "a1.m3u8"] {
        let p = hls.path().join(f);
        let s = std::fs::read_to_string(&p)
            .unwrap()
            .replace("#EXT-X-ENDLIST\n", "");
        std::fs::write(&p, s.replace("#EXT-X-PLAYLIST-TYPE:VOD\n", "")).unwrap();
    }
    let mock = MockServer::start().await;
    serve_dir(&mock, hls.path()).await;
    api_returns(&mock, 7, &format!("{}/cdn/master.m3u8", mock.uri())).await;
    let addr = start_proxy(&mock.uri(), 0).await;

    let mut resp = reqwest::get(format!("http://{addr}/tvp/7.ts"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.chunk().await.unwrap().is_some(), "got data");
    let needle = format!("tvp-7-{}", std::process::id());
    assert!(
        ffmpeg_running(&needle) >= 1,
        "ffmpeg running while streaming"
    );

    drop(resp);
    wait_until("ffmpeg to stop after the client left", || {
        ffmpeg_running(&needle) == 0
    })
    .await;
}

/// Write a shell script that stands in for ffmpeg. It lives under the target
/// dir, as /tmp may be mounted noexec, and can keep files next to itself
/// (`"$0.pid"` is `ffmpeg.with_extension("pid")`). The `TempDir` must outlive
/// the test.
fn stub_ffmpeg(script: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let bin = dir.path().join("ffmpeg");
    // Synthetic transport packets exercise lifecycle/activity, not decoding.
    // Real HLS tests above exercise the codec/demuxer boundary independently.
    let emit = r#"emit() {
        [ "${#1}" -le 184 ] || exit 1
        printf '\107\101\000\020'; printf '%-184s' "$1"
        printf '\107\101\001\020'; printf '%-184s' ''
    }"#;
    std::fs::write(&bin, format!("#!/bin/sh\n{emit}\n{script}\n")).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, bin)
}

/// Serve every `/cdn/<name>.m3u8` as a plain media playlist, so an API source
/// URL resolves to itself.
async fn serve_media(mock: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/cdn/[^/]+\.m3u8$"))
        .respond_with(ResponseTemplate::new(200).set_body_string("#EXTM3U\n#EXTINF:2,\ns.ts\n"))
        .mount(mock)
        .await;
}

/// True if a process with this pid exists.
fn running(pid: &str) -> bool {
    Command::new("ps")
        .args(["-p", pid])
        .output()
        .unwrap()
        .status
        .success()
}

#[tokio::test]
async fn client_disconnect_stops_stalled_ffmpeg() {
    // Emits once, then hangs without output like ffmpeg on a stalled upstream:
    // the disconnect must be noticed even though no more data arrives.
    let (_dir, ffmpeg) = stub_ffmpeg(r#"echo $$ > "$0.pid"; emit X; exec sleep 1000"#);
    let pid_file = ffmpeg.with_extension("pid");
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 8, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    let mut resp = reqwest::get(format!("http://{addr}/tvp/8.ts"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.chunk().await.unwrap().is_some(), "got data");
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let pid = pid.trim();
    assert!(running(pid), "ffmpeg running while streaming");

    drop(resp);
    wait_until("the stalled ffmpeg to stop after the client left", || {
        !running(pid)
    })
    .await;
}

#[tokio::test]
async fn client_disconnect_after_output_does_not_restart_ffmpeg() {
    // Even if a client leaves after the first child's output, no replacement
    // may be started on its response.
    let (_dir, ffmpeg) = stub_ffmpeg(r#"echo run >> "$0.runs"; emit X"#);
    let runs = ffmpeg.with_extension("runs");
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 9, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    let mut resp = reqwest::get(format!("http://{addr}/tvp/9.ts"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.chunk().await.unwrap().is_some(), "got data");
    drop(resp);

    tokio::time::sleep(Duration::from_millis(100)).await;
    let runs = std::fs::read_to_string(&runs).unwrap();
    assert_eq!(runs.lines().count(), 1, "ffmpeg restarted for nobody");
    let resolves = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with("/api/"))
        .count();
    assert_eq!(resolves, 1, "re-resolved for nobody");
}

#[tokio::test]
async fn client_disconnect_during_initial_resolve_releases_request() {
    let (_dir, ffmpeg) = stub_ffmpeg(&format!(r#"echo run >> "$0.runs"; {PRINT_MASTER}"#));
    let runs = ffmpeg.with_extension("runs");
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    let resolving = Arc::new(AtomicBool::new(false));
    let started = resolving.clone();
    Mock::given(path("/api/products/24/videos/playlist"))
        .respond_with(move |_: &wiremock::Request| {
            started.store(true, Ordering::SeqCst);
            ResponseTemplate::new(500).set_delay(Duration::from_secs(30))
        })
        .expect(1)
        .mount(&mock)
        .await;
    let streams = Arc::new(Semaphore::new(1));
    let state = server::AppState {
        ffmpeg,
        streams: streams.clone(),
        ..proxy_state(&mock.uri(), 0)
    };
    let shutdown = state.shutdown.clone();
    let (addr, serving) = serve_proxy(state).await;

    use tokio::io::AsyncWriteExt;
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(b"GET /tvp/24.ts HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    wait_until("the initial API request to start", || {
        resolving.load(Ordering::SeqCst)
    })
    .await;

    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(&text, "tvp_resolves_in_flight{phase=\"initial\"}"),
        1.0
    );
    assert!(
        !text.contains("channel=\"24\""),
        "unresolved requests must not allocate channel metrics"
    );

    // Initial resolution must cancel with the HTTP request, not wait for TVP.
    drop(socket);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if metric_value(&scrape_metrics(addr).await, "tvp_stream_requests_in_flight") == 0.0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    wait_until("the slot to be freed", || streams.available_permits() == 1).await;
    assert!(!runs.exists(), "child launched during unresolved request");
    mock.verify().await;

    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(&text, "tvp_resolves_in_flight{phase=\"initial\"}"),
        0.0
    );
    assert_eq!(
        metric_value(
            &text,
            "tvp_resolve_total{phase=\"initial\",outcome=\"cancelled\"}"
        ),
        1.0
    );
    assert_eq!(
        metric_value(
            &text,
            "tvp_resolve_duration_seconds_count{phase=\"initial\"}"
        ),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_stream_requests_total{outcome=\"cancelled\"}"),
        1.0
    );
    assert!(!text.contains("channel=\"24\""));
    assert_eq!(metric_value(&text, "tvp_ffmpeg_restarts_total"), 0.0);

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("server stopped")
        .unwrap()
        .unwrap();
}

/// The first line of a stream, where a stub ffmpeg printed its `-i` path.
async fn stub_master_path(resp: &mut reqwest::Response) -> PathBuf {
    assert_eq!(resp.status(), 200);
    let mut buf = Vec::new();
    while buf.len() < 188 {
        let chunk = tokio::time::timeout(Duration::from_secs(10), resp.chunk())
            .await
            .expect("stub ffmpeg output")
            .unwrap()
            .expect("stream ended early");
        buf.extend_from_slice(&chunk);
    }
    PathBuf::from(stub_text(&buf[..188]))
}

#[tokio::test]
async fn concurrent_streams_of_a_channel_use_separate_playlists() {
    // Prints the master path, then waits until that stream is told to restart.
    let (_dir, ffmpeg) = stub_ffmpeg(&format!(
        r#"{PRINT_MASTER}
        while [ "$1" != -i ]; do shift; done
        while :; do
            if [ "$(cat "$0.restart" 2>/dev/null)" = "$2" ]; then
                rm -- "$0.restart"
                exit 1
            fi
            sleep 0.1
        done"#
    ));
    let restart = ffmpeg.with_extension("restart");
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    let old = format!("{}/cdn/live.m3u8", mock.uri());
    let new = format!("{}/cdn/refreshed.m3u8", mock.uri());
    api_playlist(12, &old)
        .up_to_n_times(2)
        .expect(2)
        .mount(&mock)
        .await;
    api_playlist(12, &new).expect(0).mount(&mock).await;
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;
    let url = format!("http://{addr}/tvp/12.ts");

    let mut a = reqwest::get(&url).await.unwrap();
    let master_a = stub_master_path(&mut a).await;
    let mut b = reqwest::get(&url).await.unwrap();
    let master_b = stub_master_path(&mut b).await;
    assert_ne!(master_a, master_b, "streams share a master playlist");
    for m in [&master_a, &master_b] {
        assert!(
            m.starts_with(std::env::temp_dir()),
            "{m:?} outside work_dir"
        );
        // it holds signed CDN URLs: not readable by other users
        let mode = std::fs::metadata(m).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{m:?}");
    }

    let original_b = std::fs::read_to_string(&master_b).unwrap();
    assert_eq!(original_b.lines().last(), Some(old.as_str()));
    std::fs::write(restart, master_a.to_str().unwrap()).unwrap();
    assert!(
        read_to_end(&mut a).await.1.is_err(),
        "child exit must abort A, not append a new clock"
    );
    assert_eq!(std::fs::read_to_string(&master_b).unwrap(), original_b);

    // A ending removes its own playlist, not the one B's ffmpeg is reading
    drop(a);
    wait_until("A's playlist to be removed", || !master_a.exists()).await;
    assert!(master_b.exists(), "B's playlist removed when A ended");

    drop(b);
    wait_until("B's playlist to be removed", || !master_b.exists()).await;
    mock.verify().await;
}

/// Stub that prints the variant URL from the master playlist it reads, then fails.
const PRINT_URL: &str = r#"while [ "$1" != -i ]; do shift; done; emit "$(tail -n 1 "$2")"; exit 1"#;
/// Stub start that prints the path of the master playlist it was given; see
/// [`stub_master_path`].
const PRINT_MASTER: &str = r#"for a; do [ "$prev" = -i ] && emit "$a"; prev=$a; done"#;
/// Stub that keeps sending output until it is killed.
const TRICKLE: &str = "while :; do emit X; sleep 0.1; done";

fn stub_text(bytes: &[u8]) -> String {
    bytes
        .as_chunks::<188>()
        .0
        .iter()
        .filter(|p| p[0] == 0x47 && p[2] == 0)
        .map(|p| {
            String::from_utf8_lossy(&p[4..])
                .trim_end_matches(' ')
                .to_string()
        })
        .collect()
}

async fn read_stub_pair(response: &mut reqwest::Response) -> Vec<u8> {
    // HTTP frames and pipe reads need not coincide with 188-byte TS packets.
    // Read the whole one-shot fixture before checking its payload or counters.
    tokio::time::timeout(Duration::from_secs(2), async {
        assert_eq!(response.status(), 200);
        let mut bytes = Vec::with_capacity(376);
        while bytes.len() < 376 {
            let chunk = response.chunk().await.unwrap().expect("complete stub pair");
            bytes.extend_from_slice(&chunk);
            assert!(bytes.len() <= 376, "unexpected extra fixture payload");
        }
        bytes
    })
    .await
    .expect("complete stub pair before deadline")
}

#[tokio::test]
async fn stub_packet_pair_survives_split_http_chunks() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let mut bytes = [0; 1024];
            let n = socket.read(&mut bytes).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&bytes[..n]);
            assert!(request.len() <= 8192);
        }
        let mut pair = vec![b' '; 376];
        pair[..4].copy_from_slice(&[0x47, 0x41, 0x00, 0x10]);
        pair[4] = b'X';
        pair[188..192].copy_from_slice(&[0x47, 0x41, 0x01, 0x10]);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        for bytes in [&pair[..2], &pair[2..189], &pair[189..]] {
            socket
                .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
                .await
                .unwrap();
            socket.write_all(bytes).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
        }
        socket.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let mut response = reqwest::get(format!("http://{addr}/fixture"))
        .await
        .unwrap();
    let pair = read_stub_pair(&mut response).await;
    server.await.unwrap();
    assert_eq!(pair.len(), 376);
    assert_eq!(stub_text(&pair), "X");
}

/// Stream a channel until the proxy gives up and return the body. Giving up
/// must abort the transfer, so the client can't mistake it for a normal end.
async fn fetch(addr: SocketAddr, channel: u64) -> String {
    let body = async {
        let mut r = reqwest::get(format!("http://{addr}/tvp/{channel}.ts"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let (body, end) = read_to_end(&mut r).await;
        assert!(end.is_err(), "stream ended cleanly");
        stub_text(&body)
    };
    tokio::time::timeout(Duration::from_secs(20), body)
        .await
        .expect("stream finished")
}

#[tokio::test]
async fn post_start_exit_never_replays_input() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_playlist(10, &format!("{}/cdn/live.m3u8", mock.uri()))
        .expect(1)
        .mount(&mock)
        .await;
    let (_dir, ffmpeg) = stub_ffmpeg("emit X; exit 1");
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    assert_eq!(fetch(addr, 10).await, "X");
    mock.verify().await;
}

#[tokio::test]
async fn long_run_exit_also_aborts_without_respawn() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_playlist(11, &format!("{}/cdn/live.m3u8", mock.uri()))
        .expect(1)
        .mount(&mock)
        .await;
    // A long successful run does not grant permission to append a new clock.
    let (_dir, ffmpeg) = stub_ffmpeg(
        r#"echo >> "$0.runs"; n=$(wc -l < "$0.runs"); emit "$n"
        sleep 1; exit 1"#,
    );
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    assert_eq!(fetch(addr, 11).await, "1");
    mock.verify().await;
}

#[tokio::test]
async fn response_exit_does_not_resolve_a_new_url() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    let old = format!("{}/cdn/old.m3u8", mock.uri());
    let new = format!("{}/cdn/new.m3u8", mock.uri());
    api_playlist(13, &old)
        .up_to_n_times(1)
        .expect(1)
        .mount(&mock)
        .await;
    api_playlist(13, &new).expect(0).mount(&mock).await;
    let (_dir, ffmpeg) = stub_ffmpeg(PRINT_URL);
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    assert_eq!(fetch(addr, 13).await, old);
    mock.verify().await;
}

#[tokio::test]
async fn response_exit_does_not_retry_an_old_url() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    let old = format!("{}/cdn/old.m3u8", mock.uri());
    api_playlist(14, &old)
        .up_to_n_times(1)
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(path("/api/products/14/videos/playlist"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock)
        .await;
    let (_dir, ffmpeg) = stub_ffmpeg(PRINT_URL);
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    assert_eq!(fetch(addr, 14).await, old);
    mock.verify().await;
    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(
            &text,
            "tvp_resolve_total{phase=\"retry\",outcome=\"http_error\"}"
        ),
        0.0
    );
    assert_eq!(metric_value(&text, "tvp_ffmpeg_restarts_total"), 0.0);
    assert_eq!(
        metric_value(&text, "tvp_stream_requests_total{outcome=\"started\"}"),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"upstream_exit\"}"),
        1.0
    );
}

#[tokio::test]
async fn ffmpeg_start_failure_returns_500() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_playlist(15, &format!("{}/cdn/live.m3u8", mock.uri()))
        .expect(2)
        .mount(&mock)
        .await;
    let nowhere = PathBuf::from("/nonexistent");
    let missing_ffmpeg = server::AppState {
        ffmpeg: nowhere.join("ffmpeg"),
        ..proxy_state(&mock.uri(), 0)
    };
    let missing_work_dir = server::AppState {
        work_dir: nowhere,
        ..proxy_state(&mock.uri(), 0)
    };

    // ffmpeg is started before the 200 is sent, so the client gets an error
    // status rather than an empty stream, and there is no retry.
    for (outcome, state) in [
        ("ffmpeg_error", missing_ffmpeg),
        ("playlist_error", missing_work_dir),
    ] {
        let (addr, _) = serve_proxy(state).await;
        let r = reqwest::get(format!("http://{addr}/tvp/15.ts"))
            .await
            .unwrap();
        assert_eq!(r.status(), 500);
        let text = scrape_metrics(addr).await;
        assert_eq!(
            metric_value(
                &text,
                &format!("tvp_stream_requests_total{{outcome=\"{outcome}\"}}")
            ),
            1.0
        );
        assert_eq!(
            metric_value(&text, "tvp_stream_requests_total{outcome=\"started\"}"),
            0.0
        );
        assert_eq!(metric_value(&text, "tvp_stream_slots_in_use"), 0.0);
        assert_eq!(metric_value(&text, "tvp_stream_startup_seconds_count"), 0.0);
    }
    mock.verify().await;
}

#[tokio::test]
async fn removed_executable_after_start_does_not_trigger_a_restart() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_playlist(19, &format!("{}/cdn/live.m3u8", mock.uri()))
        .expect(1)
        .mount(&mock)
        .await;
    // deletes itself, so the restart cannot spawn it
    let (_dir, ffmpeg) = stub_ffmpeg(r#"emit X; rm -- "$0"; exit 1"#);
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    assert_eq!(fetch(addr, 19).await, "X");
    mock.verify().await;
    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"upstream_exit\"}"),
        1.0
    );
    assert_eq!(metric_value(&text, "tvp_ffmpeg_restarts_total"), 0.0);
    assert_eq!(
        metric_value(&text, "tvp_ffmpeg_processes_active{channel=\"19\"}"),
        0.0
    );
}

#[tokio::test]
async fn too_many_streams_return_503() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 20, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg(TRICKLE);
    let streams = Arc::new(Semaphore::new(1));
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        streams: streams.clone(),
        ..proxy_state(&mock.uri(), 0)
    })
    .await;
    let url = format!("http://{addr}/tvp/20.ts");

    let mut first = reqwest::get(&url).await.unwrap();
    assert_eq!(first.status(), 200);
    assert!(first.chunk().await.unwrap().is_some(), "got data");
    assert_eq!(reqwest::get(&url).await.unwrap().status(), 503);
    // a slot is taken only once the channel has resolved, so resolving never
    // holds one, and a channel that fails to resolve still gets its 502
    let unknown = format!("http://{addr}/tvp/1.ts");
    assert_eq!(reqwest::get(unknown).await.unwrap().status(), 502);

    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(&text, "tvp_stream_requests_total{outcome=\"capacity\"}"),
        1.0
    );
    assert_eq!(
        metric_value(
            &text,
            "tvp_stream_requests_total{outcome=\"resolve_error\"}"
        ),
        1.0
    );
    assert_eq!(metric_value(&text, "tvp_stream_slots_in_use"), 1.0);
    assert_eq!(
        metric_value(&text, "tvp_streams_active{channel=\"20\"}"),
        1.0
    );
    assert_eq!(
        metric_value(
            &text,
            "tvp_resolve_duration_seconds_count{phase=\"initial\"}"
        ),
        3.0
    );

    // the slot is freed once the client goes away
    drop(first);
    wait_until("the slot to be freed", || streams.available_permits() == 1).await;
    assert_eq!(reqwest::get(&url).await.unwrap().status(), 200);
}

#[tokio::test]
async fn shutdown_ends_streams() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 21, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    // Prints the master playlist path it was given, then keeps streaming.
    let (_dir, ffmpeg) = stub_ffmpeg(&format!("{PRINT_MASTER}; {TRICKLE}"));
    let state = server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    };
    let shutdown = state.shutdown.clone();
    let metrics = state.metrics.clone();
    let (addr, serving) = serve_proxy(state).await;

    let mut resp = reqwest::get(format!("http://{addr}/tvp/21.ts"))
        .await
        .unwrap();
    let master = stub_master_path(&mut resp).await;
    let needle = master.to_str().unwrap();
    assert!(
        ffmpeg_running(needle) >= 1,
        "ffmpeg running while streaming"
    );

    shutdown.cancel();
    let (_, end) = tokio::time::timeout(Duration::from_secs(5), read_to_end(&mut resp))
        .await
        .expect("stream ended");
    assert!(end.is_err(), "response ended cleanly");
    // graceful shutdown completes although a stream was running
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("server stopped")
        .unwrap()
        .unwrap();
    wait_until("the master playlist to be removed", || !master.exists()).await;
    wait_until("ffmpeg to stop", || ffmpeg_running(needle) == 0).await;
    let text = metrics.render(0, 0);
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"shutdown\"}"),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"cancelled\"}"),
        0.0
    );
    assert_eq!(
        metric_value(&text, "tvp_streams_active{channel=\"21\"}"),
        0.0
    );
    assert_eq!(
        metric_value(&text, "tvp_ffmpeg_processes_active{channel=\"21\"}"),
        0.0
    );
}

#[tokio::test]
async fn shutdown_does_not_wait_for_a_client_that_stopped_reading() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 23, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg("while :; do emit X; done");
    let streams = Arc::new(Semaphore::new(1));
    let state = server::AppState {
        ffmpeg,
        streams: streams.clone(),
        shutdown_grace: Duration::from_millis(500),
        ..proxy_state(&mock.uri(), 0)
    };
    let shutdown = state.shutdown.clone();
    let metrics = state.metrics.clone();
    let (addr, serving) = serve_proxy(state).await;

    let mut resp = reqwest::get(format!("http://{addr}/tvp/23.ts"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.chunk().await.unwrap().is_some(), "got data");
    // stop reading, and let the stream back up into the proxy
    tokio::time::sleep(Duration::from_secs(1)).await;

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("server stopped")
        .unwrap()
        .unwrap();
    // nor does the stream's task wait for the client
    wait_until("the slot to be freed", || streams.available_permits() == 1).await;
    let text = metrics.render(1, 0);
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"shutdown\"}"),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_streams_active{channel=\"23\"}"),
        0.0
    );
    assert_eq!(
        metric_value(&text, "tvp_ffmpeg_processes_active{channel=\"23\"}"),
        0.0
    );
    drop(resp);
}

#[tokio::test]
async fn resolve_without_hls_source_is_no_source() {
    let mock = MockServer::start().await;
    Mock::given(path("/api/products/4/videos/playlist"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"sources": {}})))
        .mount(&mock)
        .await;
    let r = proxy_state(&mock.uri(), 0).client.resolve(4).await;
    assert!(matches!(r, Err(tvp::ResolveError::NoSource)), "{r:?}");
}

#[tokio::test]
async fn invalid_api_json_is_reported() {
    let mock = MockServer::start().await;
    for p in ["/api/products/22/videos/playlist", "/api/products/lives"] {
        Mock::given(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>maintenance</html>"))
            .mount(&mock)
            .await;
    }
    let client = proxy_state(&mock.uri(), 0).client;
    let e = client.resolve(22).await.unwrap_err();
    assert!(matches!(e, tvp::ResolveError::Json(_)), "{e:?}");
    let e = client.lives().await.unwrap_err();
    assert!(matches!(e, tvp::ResolveError::Json(_)), "{e:?}");
}

#[tokio::test]
async fn resolve_accepts_direct_media_playlist() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    let url = format!("{}/cdn/media.m3u8", mock.uri());
    api_returns(&mock, 5, &url).await;
    let sel = proxy_state(&mock.uri(), 0).client.resolve(5).await.unwrap();
    assert_eq!(sel.video.as_str(), url);
    assert_eq!(sel.bitrate, 0);
    assert_eq!(sel.audio, None);
}

#[tokio::test]
async fn resolve_uses_redirect_target_as_base() {
    let mock = MockServer::start().await;
    Mock::given(path("/cdn/master.m3u8"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/token/x/master.m3u8", mock.uri())),
        )
        .mount(&mock)
        .await;
    Mock::given(path("/token/x/master.m3u8"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n"),
        )
        .mount(&mock)
        .await;
    Mock::given(path("/token/x/v.m3u8"))
        .respond_with(ResponseTemplate::new(200).set_body_string("#EXTM3U\n#EXTINF:2,\ns.ts\n"))
        .mount(&mock)
        .await;
    api_returns(&mock, 6, &format!("{}/cdn/master.m3u8", mock.uri())).await;
    let sel = proxy_state(&mock.uri(), 0).client.resolve(6).await.unwrap();
    assert_eq!(sel.video.as_str(), format!("{}/token/x/v.m3u8", mock.uri()));
}

#[tokio::test]
async fn resolve_rejects_oversized_response() {
    let mock = MockServer::start().await;
    Mock::given(path("/cdn/master.m3u8"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'#'; 5 * 1024 * 1024]))
        .mount(&mock)
        .await;
    api_returns(&mock, 8, &format!("{}/cdn/master.m3u8", mock.uri())).await;
    let r = proxy_state(&mock.uri(), 0).client.resolve(8).await;
    assert!(matches!(r, Err(tvp::ResolveError::TooLarge)), "{r:?}");
}

#[tokio::test]
async fn api_base_path_prefix_is_kept_without_trailing_slash() {
    let mock = MockServer::start().await;
    Mock::given(path("/prefix/api/products/lives"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [
                {"id": 1, "title": "A", "payable": true, "images": {}},
                {"id": 2, "title": "B"}
            ]
        })))
        .mount(&mock)
        .await;
    let lives = proxy_state(&format!("{}/prefix", mock.uri()), 0)
        .client
        .lives()
        .await
        .unwrap();
    let got: Vec<_> = lives.iter().map(|l| (l.id, l.payable)).collect();
    assert_eq!(got, vec![(1, true), (2, false)]);
}

/// Read the public exposition rather than relying on the collector's internals.
async fn scrape_metrics(addr: SocketAddr) -> String {
    let resp = reqwest::get(format!("http://{addr}/metrics"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    resp.text().await.unwrap()
}

fn metric_value(text: &str, sample: &str) -> f64 {
    text.lines()
        .find_map(|line| line.strip_prefix(sample)?.strip_prefix(' ')?.parse().ok())
        .unwrap_or_else(|| panic!("missing metric {sample}\n{text}"))
}

#[tokio::test]
async fn metrics_endpoint_is_self_contained() {
    let mock = MockServer::start().await;
    let (addr, _) = serve_proxy(server::AppState {
        streams: Arc::new(Semaphore::new(3)),
        ..proxy_state(&mock.uri(), 0)
    })
    .await;
    for _ in 0..2 {
        let text = scrape_metrics(addr).await;
        assert_eq!(metric_value(&text, "tvp_stream_limit"), 3.0);
        assert_eq!(metric_value(&text, "tvp_stream_slots_in_use"), 0.0);
        assert!(metric_value(&text, "tvp_process_start_time_seconds") > 0.0);
        assert!(metric_value(&text, "tvp_process_uptime_seconds") >= 0.0);
        assert_eq!(
            metric_value(
                &text,
                &format!(
                    "tvp_build_info{{version=\"{}\"}}",
                    env!("CARGO_PKG_VERSION")
                )
            ),
            1.0
        );
        assert_eq!(
            metric_value(&text, "tvp_stream_requests_total{outcome=\"started\"}"),
            0.0
        );
    }
    assert!(mock.received_requests().await.unwrap().is_empty());

    let unlimited = start_proxy(&mock.uri(), 0).await;
    assert_eq!(
        metric_value(&scrape_metrics(unlimited).await, "tvp_stream_limit"),
        0.0
    );
}

#[tokio::test]
async fn metrics_preserve_short_stream_exit_between_scrapes() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 30, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg("emit X; exit 1");
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    assert_eq!(fetch(addr, 30).await, "X");
    for _ in 0..2 {
        let text = scrape_metrics(addr).await;
        for (sample, expected) in [
            ("tvp_stream_requests_total{outcome=\"started\"}", 1.0),
            ("tvp_stream_requests_in_flight", 0.0),
            ("tvp_streams_active{channel=\"30\"}", 0.0),
            ("tvp_stream_bytes_total{channel=\"30\"}", 376.0),
            ("tvp_stream_ends_total{reason=\"upstream_exit\"}", 1.0),
            ("tvp_ffmpeg_processes_active{channel=\"30\"}", 0.0),
            ("tvp_ffmpeg_restarts_total", 0.0),
            ("tvp_resolve_total{phase=\"initial\",outcome=\"ok\"}", 1.0),
            ("tvp_resolve_total{phase=\"retry\",outcome=\"ok\"}", 0.0),
            ("tvp_resolve_duration_seconds_count{phase=\"initial\"}", 1.0),
            ("tvp_resolve_duration_seconds_count{phase=\"retry\"}", 0.0),
            (
                "tvp_resolve_duration_seconds_bucket{phase=\"retry\",le=\"+Inf\"}",
                0.0,
            ),
            ("tvp_stream_startup_seconds_count", 1.0),
            ("tvp_stream_startup_seconds_bucket{le=\"+Inf\"}", 1.0),
        ] {
            assert_eq!(metric_value(&text, sample), expected, "{sample}");
        }
        assert!(metric_value(&text, "tvp_stream_seconds_total{channel=\"30\"}") > 0.0);
        assert!(metric_value(&text, "tvp_stream_startup_seconds_sum") > 0.0);
    }
}

#[tokio::test]
async fn metrics_track_concurrent_connections_and_disconnects() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 31, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg("emit X; exec sleep 1000");
    let streams = Arc::new(Semaphore::new(2));
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        streams: streams.clone(),
        ..proxy_state(&mock.uri(), 0)
    })
    .await;
    let url = format!("http://{addr}/tvp/31.ts");
    let mut a = reqwest::get(&url).await.unwrap();
    let mut b = reqwest::get(&url).await.unwrap();
    assert_eq!(stub_text(&read_stub_pair(&mut a).await), "X");
    assert_eq!(stub_text(&read_stub_pair(&mut b).await), "X");

    let text = scrape_metrics(addr).await;
    assert_eq!(metric_value(&text, "tvp_stream_slots_in_use"), 2.0);
    assert_eq!(
        metric_value(&text, "tvp_streams_active{channel=\"31\"}"),
        2.0
    );
    assert_eq!(
        metric_value(&text, "tvp_ffmpeg_processes_active{channel=\"31\"}"),
        2.0
    );
    assert_eq!(
        metric_value(&text, "tvp_stream_bytes_total{channel=\"31\"}"),
        752.0
    );
    let ongoing = metric_value(&text, "tvp_stream_seconds_total{channel=\"31\"}");
    assert!(
        ongoing > 0.0,
        "ongoing streams must contribute before they end"
    );

    drop(a);
    wait_until("one metrics stream to end", || {
        streams.available_permits() == 1
    })
    .await;
    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(&text, "tvp_streams_active{channel=\"31\"}"),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_ffmpeg_processes_active{channel=\"31\"}"),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"client_disconnect\"}"),
        1.0
    );

    drop(b);
    wait_until("both metrics streams to end", || {
        streams.available_permits() == 2
    })
    .await;
    let text = scrape_metrics(addr).await;
    assert_eq!(metric_value(&text, "tvp_stream_slots_in_use"), 0.0);
    assert_eq!(
        metric_value(&text, "tvp_streams_active{channel=\"31\"}"),
        0.0
    );
    assert_eq!(
        metric_value(&text, "tvp_ffmpeg_processes_active{channel=\"31\"}"),
        0.0
    );
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"client_disconnect\"}"),
        2.0
    );
    assert_eq!(
        metric_value(&text, "tvp_stream_requests_total{outcome=\"started\"}"),
        2.0
    );
    assert_eq!(metric_value(&text, "tvp_stream_startup_seconds_count"), 2.0);
    assert!(metric_value(&text, "tvp_stream_seconds_total{channel=\"31\"}") >= ongoing);
}

#[tokio::test]
async fn metrics_failed_requests_do_not_allocate_channel_labels() {
    let mock = MockServer::start().await;
    let addr = start_proxy(&mock.uri(), 0).await;
    for channel in 1..=140 {
        let resp = reqwest::get(format!("http://{addr}/tvp/{channel}.ts"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }
    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(
            &text,
            "tvp_stream_requests_total{outcome=\"resolve_error\"}"
        ),
        140.0
    );
    assert_eq!(
        metric_value(
            &text,
            "tvp_resolve_total{phase=\"initial\",outcome=\"http_error\"}"
        ),
        140.0
    );
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("tvp_streams_active{"))
            .count(),
        1
    );
    assert_eq!(
        metric_value(&text, "tvp_streams_active{channel=\"other\"}"),
        0.0
    );
    assert!(
        !text.contains(&mock.uri()),
        "upstream URLs must not become labels"
    );
}

#[tokio::test]
async fn metrics_stream_without_output_has_no_startup_observation() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 32, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg("exit 1");
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;
    assert_eq!(
        reqwest::get(format!("http://{addr}/tvp/32.ts"))
            .await
            .unwrap()
            .status(),
        502
    );
    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(&text, "tvp_stream_requests_total{outcome=\"started\"}"),
        0.0
    );
    assert_eq!(metric_value(&text, "tvp_stream_startup_seconds_count"), 0.0);
    assert_eq!(
        metric_value(&text, "tvp_stream_bytes_total{channel=\"32\"}"),
        0.0
    );
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"upstream_exit\"}"),
        1.0
    );
}
