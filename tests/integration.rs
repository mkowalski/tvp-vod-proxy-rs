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
        retry_delay: Duration::from_millis(10),
        quick_failure: Duration::from_secs(30),
        streams: Arc::new(Semaphore::new(Semaphore::MAX_PERMITS)),
        shutdown: CancellationToken::new(),
        // longer than any test waits: shutdown must complete without it
        shutdown_grace: Duration::from_secs(60),
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
    // The input is a 6 s VOD at 25 fps, so every ffmpeg run ends after one pass
    // and counts as a quick failure: the client gets three passes back to back
    // before the proxy gives up. (format=duration can't tell, as timestamps
    // restart with every run.)
    let three_passes = 3 * 6 * 25;

    // no limit: top variant
    let addr = start_proxy(&mock.uri(), 0).await;
    let (streams, frames) = probe(&format!("http://{addr}/tvp/1.ts")).await;
    assert_eq!(streams, vec!["audio,2", "video,320"], "{streams:?}");
    assert_eq!(frames, three_passes);

    // capped: lower variant
    let addr = start_proxy(&mock.uri(), 300_000).await;
    let (streams, frames) = probe(&format!("http://{addr}/tvp/1.ts")).await;
    assert_eq!(streams, vec!["audio,2", "video,160"], "{streams:?}");
    assert_eq!(frames, three_passes);
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
    std::fs::write(&bin, format!("#!/bin/sh\n{script}\n")).unwrap();
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
    let (_dir, ffmpeg) = stub_ffmpeg(r#"echo $$ > "$0.pid"; printf X; exec sleep 1000"#);
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
async fn client_disconnect_during_retry_does_not_restart_ffmpeg() {
    // ffmpeg exits at once, so the proxy waits `retry_delay` and re-resolves;
    // a client that leaves meanwhile must not get a fresh ffmpeg.
    let (_dir, ffmpeg) = stub_ffmpeg(r#"echo run >> "$0.runs"; printf X"#);
    let runs = ffmpeg.with_extension("runs");
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 9, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let retry_delay = Duration::from_secs(1);
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        retry_delay,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    let mut resp = reqwest::get(format!("http://{addr}/tvp/9.ts"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.chunk().await.unwrap().is_some(), "got data");
    drop(resp);

    tokio::time::sleep(retry_delay * 2).await;
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
async fn client_disconnect_during_re_resolve_releases_stream() {
    let (_dir, ffmpeg) = stub_ffmpeg(&format!(r#"echo run >> "$0.runs"; {PRINT_MASTER}"#));
    let runs = ffmpeg.with_extension("runs");
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_playlist(24, &format!("{}/cdn/live.m3u8", mock.uri()))
        .up_to_n_times(1)
        .expect(1)
        .mount(&mock)
        .await;
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

    let mut resp = reqwest::get(format!("http://{addr}/tvp/24.ts"))
        .await
        .unwrap();
    let master = stub_master_path(&mut resp).await;
    wait_until("the second API request to start", || {
        resolving.load(Ordering::SeqCst)
    })
    .await;

    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(&text, "tvp_resolves_in_flight{phase=\"retry\"}"),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_streams_active{channel=\"24\"}"),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_ffmpeg_processes_active{channel=\"24\"}"),
        0.0
    );

    // Leave while the API request is pending, not during the retry sleep.
    // Cleanup must not wait for the upstream response or its 20 s timeout.
    drop(resp);
    wait_until("the master playlist to be removed", || !master.exists()).await;
    wait_until("the slot to be freed", || streams.available_permits() == 1).await;
    assert_eq!(std::fs::read_to_string(runs).unwrap().lines().count(), 1);
    mock.verify().await;

    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(&text, "tvp_resolves_in_flight{phase=\"retry\"}"),
        0.0
    );
    assert_eq!(
        metric_value(
            &text,
            "tvp_resolve_total{phase=\"retry\",outcome=\"cancelled\"}"
        ),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_resolve_duration_seconds_count{phase=\"retry\"}"),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"client_disconnect\"}"),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_streams_active{channel=\"24\"}"),
        0.0
    );
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
    while !buf.contains(&b'\n') {
        let chunk = tokio::time::timeout(Duration::from_secs(10), resp.chunk())
            .await
            .expect("stub ffmpeg output")
            .unwrap()
            .expect("stream ended early");
        buf.extend_from_slice(&chunk);
    }
    let line = buf.split(|&b| b == b'\n').next().unwrap();
    PathBuf::from(String::from_utf8(line.to_vec()).unwrap())
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
    api_playlist(12, &new).expect(1).mount(&mock).await;
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
    assert_eq!(stub_master_path(&mut a).await, master_a);
    let refreshed_a = std::fs::read_to_string(&master_a).unwrap();
    assert_eq!(refreshed_a.lines().last(), Some(new.as_str()));
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
const PRINT_URL: &str = r#"while [ "$1" != -i ]; do shift; done; tail -n 1 "$2"; exit 1"#;
/// Stub start that prints the path of the master playlist it was given; see
/// [`stub_master_path`].
const PRINT_MASTER: &str = r#"for a; do [ "$prev" = -i ] && echo "$a"; prev=$a; done"#;
/// Stub that keeps sending output until it is killed.
const TRICKLE: &str = "while :; do printf X; sleep 0.1; done";

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
        String::from_utf8(body).unwrap()
    };
    tokio::time::timeout(Duration::from_secs(20), body)
        .await
        .expect("stream finished")
}

#[tokio::test]
async fn gives_up_after_three_quick_failures() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_playlist(10, &format!("{}/cdn/live.m3u8", mock.uri()))
        .expect(3)
        .mount(&mock)
        .await;
    let (_dir, ffmpeg) = stub_ffmpeg("printf X; exit 1");
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    // the first run plus two restarts, each after re-resolving
    assert_eq!(fetch(addr, 10).await, "XXX");
    mock.verify().await;
}

#[tokio::test]
async fn long_run_resets_quick_failure_count() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_playlist(11, &format!("{}/cdn/live.m3u8", mock.uri()))
        .expect(6)
        .mount(&mock)
        .await;
    // prints its run number; only run 3 outlasts `quick_failure`
    let (_dir, ffmpeg) = stub_ffmpeg(
        r#"echo >> "$0.runs"; n=$(wc -l < "$0.runs"); printf $n
        if [ $n = 3 ]; then sleep 2; fi; exit 1"#,
    );
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        quick_failure: Duration::from_secs(1),
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    // 1 and 2 fail quickly, 3 resets the count, 4 to 6 fail quickly again
    assert_eq!(fetch(addr, 11).await, "123456");
    mock.verify().await;
}

#[tokio::test]
async fn restart_uses_re_resolved_url() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    let old = format!("{}/cdn/old.m3u8", mock.uri());
    let new = format!("{}/cdn/new.m3u8", mock.uri());
    api_playlist(13, &old)
        .up_to_n_times(1)
        .expect(1)
        .mount(&mock)
        .await;
    api_playlist(13, &new).expect(2).mount(&mock).await;
    let (_dir, ffmpeg) = stub_ffmpeg(PRINT_URL);
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    assert_eq!(fetch(addr, 13).await, format!("{old}\n{new}\n{new}\n"));
    mock.verify().await;
}

#[tokio::test]
async fn restart_keeps_old_url_if_re_resolve_fails() {
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
        .expect(2)
        .mount(&mock)
        .await;
    let (_dir, ffmpeg) = stub_ffmpeg(PRINT_URL);
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    assert_eq!(fetch(addr, 14).await, format!("{old}\n{old}\n{old}\n"));
    mock.verify().await;
    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(
            &text,
            "tvp_resolve_total{phase=\"retry\",outcome=\"http_error\"}"
        ),
        2.0
    );
    assert_eq!(metric_value(&text, "tvp_ffmpeg_restarts_total"), 2.0);
    assert_eq!(
        metric_value(&text, "tvp_stream_requests_total{outcome=\"started\"}"),
        1.0
    );
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"quick_failures\"}"),
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
async fn restart_failure_aborts_the_response() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_playlist(19, &format!("{}/cdn/live.m3u8", mock.uri()))
        .expect(2)
        .mount(&mock)
        .await;
    // deletes itself, so the restart cannot spawn it
    let (_dir, ffmpeg) = stub_ffmpeg(r#"printf X; rm -- "$0"; exit 1"#);
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    assert_eq!(fetch(addr, 19).await, "X");
    mock.verify().await;
    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"restart_error\"}"),
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
    let (_dir, ffmpeg) = stub_ffmpeg("exec dd if=/dev/zero bs=65536 2>/dev/null");
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
async fn metrics_preserve_short_streams_and_restarts_between_scrapes() {
    let mock = MockServer::start().await;
    serve_media(&mock).await;
    api_returns(&mock, 30, &format!("{}/cdn/live.m3u8", mock.uri())).await;
    let (_dir, ffmpeg) = stub_ffmpeg("printf X; exit 1");
    let (addr, _) = serve_proxy(server::AppState {
        ffmpeg,
        ..proxy_state(&mock.uri(), 0)
    })
    .await;

    assert_eq!(fetch(addr, 30).await, "XXX");
    for _ in 0..2 {
        let text = scrape_metrics(addr).await;
        for (sample, expected) in [
            ("tvp_stream_requests_total{outcome=\"started\"}", 1.0),
            ("tvp_stream_requests_in_flight", 0.0),
            ("tvp_streams_active{channel=\"30\"}", 0.0),
            ("tvp_stream_bytes_total{channel=\"30\"}", 3.0),
            ("tvp_stream_ends_total{reason=\"quick_failures\"}", 1.0),
            ("tvp_ffmpeg_processes_active{channel=\"30\"}", 0.0),
            ("tvp_ffmpeg_restarts_total", 2.0),
            ("tvp_resolve_total{phase=\"initial\",outcome=\"ok\"}", 1.0),
            ("tvp_resolve_total{phase=\"retry\",outcome=\"ok\"}", 2.0),
            ("tvp_resolve_duration_seconds_count{phase=\"initial\"}", 1.0),
            ("tvp_resolve_duration_seconds_count{phase=\"retry\"}", 2.0),
            (
                "tvp_resolve_duration_seconds_bucket{phase=\"retry\",le=\"+Inf\"}",
                2.0,
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
    let (_dir, ffmpeg) = stub_ffmpeg("printf X; exec sleep 1000");
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
    assert_eq!(&a.chunk().await.unwrap().unwrap()[..], b"X");
    assert_eq!(&b.chunk().await.unwrap().unwrap()[..], b"X");

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
        2.0
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
    assert_eq!(fetch(addr, 32).await, "");
    let text = scrape_metrics(addr).await;
    assert_eq!(
        metric_value(&text, "tvp_stream_requests_total{outcome=\"started\"}"),
        1.0
    );
    assert_eq!(metric_value(&text, "tvp_stream_startup_seconds_count"), 0.0);
    assert_eq!(
        metric_value(&text, "tvp_stream_bytes_total{channel=\"32\"}"),
        0.0
    );
    assert_eq!(
        metric_value(&text, "tvp_stream_ends_total{reason=\"quick_failures\"}"),
        1.0
    );
}
