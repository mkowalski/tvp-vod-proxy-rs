//! End-to-end tests against a mock TVP API/CDN. Streaming tests need ffmpeg and
//! ffprobe on PATH and are skipped (with a message) if they are missing.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tvp_vod_proxy::{server, tvp};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
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

async fn api_returns(mock: &MockServer, channel: u64, master_url: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/api/products/{channel}/videos/playlist")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sources": {"HLS": [{"src": master_url}]}
        })))
        .mount(mock)
        .await;
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

async fn start_proxy(api: &str, max_bitrate: u64) -> SocketAddr {
    let client = tvp::Client::new(Url::parse(api).unwrap(), max_bitrate).unwrap();
    let state = server::AppState {
        client,
        ffmpeg: PathBuf::from("ffmpeg"),
        work_dir: std::env::temp_dir(),
        retry_delay: Duration::from_millis(10),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, server::router(state)).await });
    addr
}

/// Download a stream (bounded by time) and return ffprobe's stream summary.
async fn probe(url: &str) -> Vec<String> {
    let bytes = tokio::time::timeout(Duration::from_secs(60), async {
        reqwest::get(url).await.unwrap().bytes().await.unwrap()
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
    lines
}

#[tokio::test]
async fn streams_one_video_and_one_audio_as_mpegts() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found");
        return;
    }
    let hls = tempfile::tempdir().unwrap();
    make_hls(hls.path());
    let mock = MockServer::start().await;
    serve_dir(&mock, hls.path()).await;
    api_returns(&mock, 1, &format!("{}/cdn/master.m3u8", mock.uri())).await;

    // no limit: top variant
    let addr = start_proxy(&mock.uri(), 0).await;
    let streams = probe(&format!("http://{addr}/tvp/1.ts")).await;
    assert_eq!(streams, vec!["audio,2", "video,320"], "{streams:?}");

    // capped: lower variant
    let addr = start_proxy(&mock.uri(), 300_000).await;
    let streams = probe(&format!("http://{addr}/tvp/1.ts")).await;
    assert_eq!(streams, vec!["audio,2", "video,160"], "{streams:?}");
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

#[tokio::test]
async fn client_disconnect_stops_ffmpeg() {
    if !have("ffmpeg") {
        eprintln!("skipping: ffmpeg not found");
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
    for _ in 0..50 {
        if ffmpeg_running(&needle) == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("ffmpeg still running 5s after client disconnected");
}
