//! Client for TVP's VOD API: resolve a live channel to a playable selection.

use crate::hls::{self, Selection};
use serde::Deserialize;
use std::time::Duration;
use url::Url;

/// Default `TVP_API` base URL.
pub const DEFAULT_API: &str = "https://vod.tvp.pl";
/// Upper bound on any API or playlist response body.
const MAX_BODY: usize = 4 * 1024 * 1024;
/// `maxResults` requested from the lives endpoint.
const MAX_LIVES: usize = 500;
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// Why a channel could not be resolved to a playable stream.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("channel is DRM-protected")]
    Drm,
    #[error("no HLS source for channel")]
    NoSource,
    #[error("no playable variant in master playlist")]
    NoVariant,
    #[error("invalid API response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("upstream: {0}")]
    Upstream(#[from] reqwest::Error),
    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("upstream response larger than {MAX_BODY} bytes")]
    TooLarge,
}

/// HTTP client for the TVP API and CDN playlists.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    api: Url,
    max_bitrate: u64,
}

#[derive(Deserialize)]
struct Playlist {
    sources: Option<Sources>,
}

#[derive(Deserialize)]
struct Sources {
    #[serde(rename = "HLS", default)]
    hls: Vec<Source>,
}

#[derive(Deserialize)]
struct Source {
    src: String,
}

/// One entry of the API's live channel list.
#[derive(Debug, Deserialize)]
pub struct LiveItem {
    /// Channel id, as used in `/tvp/<id>.ts`.
    pub id: u64,
    pub title: String,
    /// Requires a TVP subscription.
    #[serde(default)]
    pub payable: bool,
    /// Raw `images` object; see [`crate::m3u::logo`].
    #[serde(default)]
    pub images: serde_json::Value,
}

#[derive(Deserialize)]
struct Lives {
    items: Vec<LiveItem>,
}

impl Client {
    /// `api` is the API base URL; a path prefix is kept even without a
    /// trailing slash. `max_bitrate` is in bit/s, 0 = no limit.
    pub fn new(mut api: Url, max_bitrate: u64) -> reqwest::Result<Self> {
        // `Url::join` replaces the last path segment unless it ends in '/'
        if !api.path().ends_with('/') {
            let path = format!("{}/", api.path());
            api.set_path(&path);
        }
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            api,
            max_bitrate,
        })
    }

    async fn get(&self, url: Url) -> Result<(Url, String), ResolveError> {
        let mut resp = self.http.get(url).send().await?.error_for_status()?;
        let final_url = resp.url().clone();
        if resp.content_length().is_some_and(|n| n > MAX_BODY as u64) {
            return Err(ResolveError::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(ResolveError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((final_url, String::from_utf8_lossy(&body).into_owned()))
    }

    /// Resolve a live channel id to the variant ffmpeg should read.
    pub async fn resolve(&self, channel: u64) -> Result<Selection, ResolveError> {
        let mut api = self
            .api
            .join(&format!("api/products/{channel}/videos/playlist"))?;
        api.query_pairs_mut()
            .append_pair("platform", "BROWSER")
            .append_pair("videoType", "LIVE");
        let (_, body) = self.get(api).await?;
        let playlist: Playlist = serde_json::from_str(&body)?;
        let src = playlist
            .sources
            .and_then(|s| s.hls.into_iter().next())
            .ok_or(ResolveError::NoSource)?;

        let (base, master) = self.get(Url::parse(&src.src)?).await?;
        if hls::is_encrypted(&master) {
            return Err(ResolveError::Drm);
        }
        let sel = match hls::select(&master, &base, self.max_bitrate) {
            Some(sel) => sel,
            None if hls::is_master(&master) => return Err(ResolveError::NoVariant),
            // the URL already points at a media playlist
            None => {
                return Ok(Selection {
                    bitrate: 0,
                    video: base,
                    audio: None,
                });
            }
        };
        for url in std::iter::once(&sel.video).chain(&sel.audio) {
            let (_, media) = self.get(url.clone()).await?;
            if hls::is_encrypted(&media) {
                return Err(ResolveError::Drm);
            }
        }
        Ok(sel)
    }

    /// All live channels listed by the API.
    pub async fn lives(&self) -> Result<Vec<LiveItem>, ResolveError> {
        let mut url = self.api.join("api/products/lives")?;
        url.query_pairs_mut()
            .append_pair("lang", "PL")
            .append_pair("platform", "BROWSER")
            .append_pair("maxResults", &MAX_LIVES.to_string());
        let (_, body) = self.get(url).await?;
        let lives: Lives = serde_json::from_str(&body)?;
        if lives.items.len() >= MAX_LIVES {
            tracing::warn!(
                count = lives.items.len(),
                "live channel list may be truncated"
            );
        }
        Ok(lives.items)
    }
}
