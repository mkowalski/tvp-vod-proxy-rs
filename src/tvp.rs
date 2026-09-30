//! Client for TVP's VOD API: resolve a live channel to a playable selection.

use crate::hls::{self, Selection};
use serde::Deserialize;
use std::time::Duration;
use url::Url;

pub const DEFAULT_API: &str = "https://vod.tvp.pl";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("channel is DRM-protected")]
    Drm,
    #[error("no HLS source for channel")]
    NoSource,
    #[error("upstream: {0}")]
    Upstream(#[from] reqwest::Error),
    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),
}

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

#[derive(Debug, Deserialize)]
pub struct LiveItem {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub payable: bool,
    #[serde(default)]
    pub images: serde_json::Value,
}

#[derive(Deserialize)]
struct Lives {
    items: Vec<LiveItem>,
}

impl Client {
    pub fn new(api: Url, max_bitrate: u64) -> reqwest::Result<Self> {
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
        let resp = self.http.get(url).send().await?.error_for_status()?;
        let final_url = resp.url().clone();
        Ok((final_url, resp.text().await?))
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
        let playlist: Playlist = serde_json::from_str(&body).map_err(|_| ResolveError::NoSource)?;
        let src = playlist
            .sources
            .and_then(|s| s.hls.into_iter().next())
            .ok_or(ResolveError::NoSource)?;

        let (base, master) = self.get(Url::parse(&src.src)?).await?;
        let sel = match hls::select(&master, &base, self.max_bitrate) {
            Some(sel) => sel,
            // the URL already points at a media playlist
            None => {
                if hls::is_encrypted(&master) {
                    return Err(ResolveError::Drm);
                }
                return Ok(Selection {
                    bitrate: 0,
                    video: base,
                    audio: None,
                });
            }
        };
        let (_, media) = self.get(sel.video.clone()).await?;
        if hls::is_encrypted(&media) {
            return Err(ResolveError::Drm);
        }
        Ok(sel)
    }

    /// All live channels listed by the API.
    pub async fn lives(&self) -> Result<Vec<LiveItem>, ResolveError> {
        let mut url = self.api.join("api/products/lives")?;
        url.query_pairs_mut()
            .append_pair("lang", "PL")
            .append_pair("platform", "BROWSER")
            .append_pair("maxResults", "500");
        let (_, body) = self.get(url).await?;
        let lives: Lives = serde_json::from_str(&body).map_err(|_| ResolveError::NoSource)?;
        Ok(lives.items)
    }
}
