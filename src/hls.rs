//! HLS master playlist handling: pick one video variant and its default audio
//! rendition, detect DRM, and write a single-variant master for ffmpeg.

use url::Url;

/// A playable variant chosen from a master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Average (or peak, if no average is given) bitrate in bits/s.
    pub bitrate: u64,
    /// Absolute URL of the video media playlist.
    pub video: Url,
    /// Absolute URL of the separate audio media playlist, if the variant has one.
    pub audio: Option<Url>,
}

#[derive(Debug, Clone)]
struct Variant {
    bitrate: u64,
    uri: String,
    audio_group: Option<String>,
}

/// Value of `KEY=` in an `#EXT-X-...:` attribute list. Handles quoted values.
/// Malformed tokens (no `=`, unterminated quote) are skipped rather than
/// invalidating the whole line.
fn attr(line: &str, key: &str) -> Option<String> {
    let body = line.split_once(':')?.1;
    let mut rest = body;
    while !rest.is_empty() {
        let (name, after) = rest.split_once('=')?;
        // tokens without '=' before this one end up in `name`: drop them
        let name = name.rsplit_once(',').map_or(name, |(_, n)| n);
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => (&quoted[..end], quoted[end + 1..].trim_start_matches(',')),
                // unterminated quote: treat it as a plain value
                None => quoted.split_once(',').unwrap_or((quoted, "")),
            }
        } else {
            match after.split_once(',') {
                Some((v, n)) => (v, n),
                None => (after, ""),
            }
        };
        if name.trim().eq_ignore_ascii_case(key) {
            return Some(value.to_string());
        }
        rest = next;
    }
    None
}

fn variants(master: &str) -> Vec<Variant> {
    let lines: Vec<&str> = master.lines().map(str::trim).collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.starts_with("#EXT-X-STREAM-INF:") {
            continue;
        }
        let Some(uri) = lines[i + 1..]
            .iter()
            .find(|l| !l.is_empty() && !l.starts_with('#'))
        else {
            continue;
        };
        let rate = attr(line, "AVERAGE-BANDWIDTH")
            .or_else(|| attr(line, "BANDWIDTH"))
            .and_then(|v| v.parse().ok());
        if let Some(bitrate) = rate {
            out.push(Variant {
                bitrate,
                uri: (*uri).to_string(),
                audio_group: attr(line, "AUDIO"),
            });
        }
    }
    out
}

/// Choose the highest-bitrate variant not above `max_bitrate` (0 = no limit).
/// If every variant exceeds the cap, the lowest one is used.
/// Returns `None` if the playlist has no variants (i.e. it is a media playlist).
pub fn select(master: &str, base: &Url, max_bitrate: u64) -> Option<Selection> {
    let mut all = variants(master);
    all.sort_by_key(|v| v.bitrate);
    let chosen = if max_bitrate == 0 {
        all.last()
    } else {
        all.iter()
            .rev()
            .find(|v| v.bitrate <= max_bitrate)
            .or(all.first())
    }?;

    let audio = chosen.audio_group.as_ref().and_then(|group| {
        let renditions: Vec<&str> = master
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("#EXT-X-MEDIA:"))
            .filter(|l| attr(l, "TYPE").as_deref() == Some("AUDIO"))
            .filter(|l| attr(l, "GROUP-ID").as_deref() == Some(group))
            .filter(|l| attr(l, "URI").is_some())
            .collect();
        let default = renditions
            .iter()
            .find(|l| attr(l, "DEFAULT").is_some_and(|d| d.eq_ignore_ascii_case("YES")))
            .or(renditions.first())?;
        base.join(&attr(default, "URI")?).ok()
    });

    Some(Selection {
        bitrate: chosen.bitrate,
        video: base.join(&chosen.uri).ok()?,
        audio,
    })
}

/// True if a media playlist is encrypted (FairPlay/Widevine/AES).
pub fn is_encrypted(media: &str) -> bool {
    media
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("#EXT-X-KEY:") || l.starts_with("#EXT-X-SESSION-KEY:"))
        .any(|l| attr(l, "METHOD").is_some_and(|m| !m.eq_ignore_ascii_case("NONE")))
}

/// A master playlist with exactly one variant, so ffmpeg reads video and audio
/// as one program on one timeline. (Two separate `-i` inputs get their
/// timestamps zeroed independently and drift out of A/V sync.)
pub fn single_variant_master(sel: &Selection) -> String {
    let mut out = String::from("#EXTM3U\n");
    if let Some(audio) = &sel.audio {
        out.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",DEFAULT=YES,AUTOSELECT=YES,NAME=\"audio\",URI=\"{audio}\"\n"
        ));
        out.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={},AUDIO=\"a\"\n",
            sel.bitrate
        ));
    } else {
        out.push_str(&format!("#EXT-X-STREAM-INF:BANDWIDTH={}\n", sel.bitrate));
    }
    out.push_str(sel.video.as_str());
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TVP: &str = include_str!("../tests/fixtures/tvp_3variants.m3u8");
    const BROADPEAK: &str = include_str!("../tests/fixtures/broadpeak_no_avg.m3u8");
    const REDCDN: &str = include_str!("../tests/fixtures/redcdn_absolute.m3u8");
    const DRM: &str = include_str!("../tests/fixtures/media_drm.m3u8");

    fn base() -> Url {
        Url::parse("https://cdn.example/token/abc/156/master.m3u8").unwrap()
    }

    #[test]
    fn no_limit_picks_top_variant() {
        let s = select(TVP, &base(), 0).unwrap();
        assert_eq!(s.bitrate, 6_811_200);
        assert_eq!(
            s.video.as_str(),
            "https://cdn.example/token/abc/156/master_v1.m3u8"
        );
        assert_eq!(
            s.audio.unwrap().as_str(),
            "https://cdn.example/token/abc/156/master_a1.m3u8"
        );
    }

    #[test]
    fn cap_picks_highest_under_limit() {
        let s = select(TVP, &base(), 4_000_000).unwrap();
        assert_eq!(s.bitrate, 2_741_200);
        assert!(s.video.as_str().ends_with("master_v2.m3u8"));
    }

    #[test]
    fn cap_below_all_picks_lowest() {
        let s = select(TVP, &base(), 100_000).unwrap();
        assert_eq!(s.bitrate, 761_200);
    }

    #[test]
    fn iframe_playlists_are_ignored() {
        let s = select(TVP, &base(), 0).unwrap();
        assert!(!s.video.as_str().contains("I-Frame"));
    }

    #[test]
    fn falls_back_to_bandwidth_and_first_audio() {
        let s = select(BROADPEAK, &base(), 0).unwrap();
        assert_eq!(s.bitrate, 7_680_000);
        assert!(s.video.as_str().ends_with("video=7200000.m3u8"));
        assert!(s.audio.unwrap().as_str().ends_with("audio_pol=211000.m3u8"));
    }

    #[test]
    fn prefers_default_audio_and_keeps_absolute_urls() {
        let s = select(REDCDN, &base(), 0).unwrap();
        assert_eq!(
            s.video.as_str(),
            "https://cdn.example/live/playlist.m3u8?videoId=1"
        );
        assert!(s.audio.unwrap().as_str().ends_with("audioId=1&lang=pol"));
    }

    #[test]
    fn media_playlist_has_no_selection() {
        assert_eq!(select(DRM, &base(), 0), None);
    }

    #[test]
    fn detects_drm() {
        assert!(is_encrypted(DRM));
        assert!(!is_encrypted("#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n"));
        assert!(!is_encrypted("#EXTM3U\n#EXTINF:2.0,\nseg.mp4\n"));
    }

    #[test]
    fn single_variant_master_with_audio() {
        let s = select(TVP, &base(), 0).unwrap();
        let m = single_variant_master(&s);
        assert_eq!(m.matches("#EXT-X-STREAM-INF").count(), 1);
        assert!(m.contains("URI=\"https://cdn.example/token/abc/156/master_a1.m3u8\""));
        assert!(m.contains("AUDIO=\"a\""));
        assert!(m.trim_end().ends_with("master_v1.m3u8"));
    }

    #[test]
    fn single_variant_master_without_audio() {
        let s = Selection {
            bitrate: 1,
            video: base(),
            audio: None,
        };
        let m = single_variant_master(&s);
        assert!(!m.contains("EXT-X-MEDIA"));
        assert!(!m.contains("AUDIO="));
    }

    #[test]
    fn attr_parses_quoted_and_plain() {
        let l = r#"#EXT-X-STREAM-INF:BANDWIDTH=10,CODECS="a,b",AUDIO="g""#;
        assert_eq!(attr(l, "BANDWIDTH").as_deref(), Some("10"));
        assert_eq!(attr(l, "CODECS").as_deref(), Some("a,b"));
        assert_eq!(attr(l, "AUDIO").as_deref(), Some("g"));
        assert_eq!(attr(l, "MISSING"), None);
    }

    #[test]
    fn attr_skips_malformed_tokens() {
        let l = r#"#EXT-X-STREAM-INF:JUNK,BANDWIDTH=10,AUDIO="g"#;
        assert_eq!(attr(l, "BANDWIDTH").as_deref(), Some("10"));
        assert_eq!(attr(l, "AUDIO").as_deref(), Some("g"));
        let l = r#"#EXT-X-STREAM-INF:CODECS="a"junk,BANDWIDTH=5"#;
        assert_eq!(attr(l, "BANDWIDTH").as_deref(), Some("5"));
    }

    #[test]
    fn malformed_attribute_does_not_drop_variant() {
        let m = "#EXTM3U\n#EXT-X-STREAM-INF:ODD,BANDWIDTH=1000,RESOLUTION=1x1\nv.m3u8\n";
        let s = select(m, &base(), 0).unwrap();
        assert_eq!(s.bitrate, 1000);
        assert!(s.video.as_str().ends_with("/v.m3u8"));
    }

    #[test]
    fn cap_is_inclusive() {
        let s = select(TVP, &base(), 2_741_200).unwrap();
        assert_eq!(s.bitrate, 2_741_200);
        let s = select(TVP, &base(), 2_741_199).unwrap();
        assert_eq!(s.bitrate, 761_200);
    }

    #[test]
    fn muxed_audio_has_no_separate_rendition() {
        let m = "#EXTM3U\n\
                 #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"pl\",DEFAULT=YES\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=1000,AUDIO=\"a\"\n\
                 v.m3u8\n";
        let s = select(m, &base(), 0).unwrap();
        assert_eq!(s.audio, None);
    }
}
