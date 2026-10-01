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
    url: Url,
    audio_group: Option<String>,
}

/// `CODECS` prefixes (RFC 6381 sample entry codes) of video codecs.
const VIDEO_CODECS: [&str; 10] = [
    "avc1", "avc3", "hvc1", "hev1", "dvh1", "dvhe", "dva1", "dvav", "av01", "vp09",
];

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

/// `uri` resolved against `base`, if that gives an http(s) URL. Playlists must
/// not point ffmpeg at local files or other protocols.
fn http_url(base: &Url, uri: &str) -> Option<Url> {
    base.join(uri)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
}

/// False if the variant's `CODECS` lists no video codec (an audio-only
/// variant). Without `CODECS` we can't tell and assume it has video.
fn has_video(stream_inf: &str) -> bool {
    attr(stream_inf, "CODECS").is_none_or(|codecs| {
        codecs
            .split(',')
            .any(|c| VIDEO_CODECS.iter().any(|v| c.trim().starts_with(v)))
    })
}

/// Playable video variants of a master playlist.
fn variants(master: &str, base: &Url) -> Vec<Variant> {
    // tags and URIs only: blank lines and comments are ignored (RFC 8216 4.1)
    let mut lines = master
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && (!l.starts_with('#') || l.starts_with("#EXT")))
        .peekable();
    let mut out = Vec::new();
    while let Some(line) = lines.next() {
        if !line.starts_with("#EXT-X-STREAM-INF:") {
            continue;
        }
        // the URI must come next; a tag there means it is missing
        let Some(uri) = lines.next_if(|l| !l.starts_with('#')) else {
            continue;
        };
        let rate = attr(line, "AVERAGE-BANDWIDTH")
            .or_else(|| attr(line, "BANDWIDTH"))
            .and_then(|v| v.parse().ok());
        let (Some(bitrate), Some(url)) = (rate, http_url(base, uri)) else {
            continue;
        };
        if has_video(line) {
            out.push(Variant {
                bitrate,
                url,
                audio_group: attr(line, "AUDIO"),
            });
        }
    }
    out
}

/// True if the playlist is a master playlist, i.e. it lists variant streams.
pub fn is_master(playlist: &str) -> bool {
    playlist
        .lines()
        .any(|l| l.trim().starts_with("#EXT-X-STREAM-INF:"))
}

/// Choose the highest-bitrate variant not above `max_bitrate` (0 = no limit).
/// If every variant exceeds the cap, the lowest one is used.
/// Returns `None` if the playlist has no usable variant: it is a media playlist
/// (see [`is_master`]) or a master whose variants all lack a bitrate, a valid
/// http(s) URI or video.
pub fn select(master: &str, base: &Url, max_bitrate: u64) -> Option<Selection> {
    let mut all = variants(master, base);
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
        master
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("#EXT-X-MEDIA:"))
            .filter(|l| attr(l, "TYPE").as_deref() == Some("AUDIO"))
            .filter(|l| attr(l, "GROUP-ID").as_deref() == Some(group))
            .filter_map(|l| Some((l, http_url(base, &attr(l, "URI")?)?)))
            // DEFAULT=YES, else AUTOSELECT=YES, else the first listed;
            // audio description only if there is nothing else
            .min_by_key(|(l, _)| {
                let yes = |key| attr(l, key).is_some_and(|v| v.eq_ignore_ascii_case("YES"));
                let described = attr(l, "CHARACTERISTICS")
                    .is_some_and(|c| c.contains("public.accessibility.describes-video"));
                (described, !yes("DEFAULT"), !yes("AUTOSELECT"))
            })
            .map(|(_, url)| url)
    });

    Some(Selection {
        bitrate: chosen.bitrate,
        video: chosen.url.clone(),
        audio,
    })
}

/// True if a playlist is encrypted (FairPlay/Widevine/AES): `EXT-X-KEY` in a
/// media playlist or `EXT-X-SESSION-KEY` in a master playlist.
pub fn is_encrypted(playlist: &str) -> bool {
    playlist
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
        assert!(!is_master(DRM));
        assert!(is_master(TVP));
    }

    #[test]
    fn detects_drm() {
        assert!(is_encrypted(DRM));
        assert!(!is_encrypted("#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n"));
        assert!(!is_encrypted("#EXTM3U\n#EXTINF:2.0,\nseg.mp4\n"));
    }

    #[test]
    fn detects_drm_in_master() {
        let m = format!("{TVP}#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n");
        assert!(is_encrypted(&m));
        assert!(!is_encrypted(TVP));
    }

    #[test]
    fn variant_without_uri_does_not_take_the_next_one() {
        let m = "#EXTM3U\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=9000000\n\
                 \n\
                 #EXT-X-STREAM-INF:BANDWIDTH=1000\n\
                 # a comment\n\
                 low.m3u8\n";
        for cap in [0, 5_000_000] {
            let s = select(m, &base(), cap).unwrap();
            assert_eq!(s.bitrate, 1000);
            assert!(s.video.as_str().ends_with("/low.m3u8"));
        }
    }

    #[test]
    fn audio_only_variants_are_skipped() {
        let m = "#EXTM3U\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS=\"mp4a.40.2\"\n\
                 audio.m3u8\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"mp4a.40.2,hvc1.1.6.L93.B0\"\n\
                 video.m3u8\n";
        let s = select(m, &base(), 100_000).unwrap();
        assert!(s.video.as_str().ends_with("/video.m3u8"));
        assert!(is_master(m));
        let audio_only = m.replace("hvc1.1.6.L93.B0", "ec-3");
        assert_eq!(select(&audio_only, &base(), 0), None);
    }

    #[test]
    fn invalid_variants_give_no_selection() {
        for m in [
            "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1x1\nv.m3u8\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n#EXT-X-ENDLIST\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nhttp://[::1\n",
        ] {
            assert!(is_master(m), "{m}");
            assert_eq!(select(m, &base(), 0), None, "{m}");
        }
    }

    #[test]
    fn only_http_urls_are_used() {
        let m = "#EXTM3U\n\
                 #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",DEFAULT=YES,URI=\"file:///etc/passwd\"\n\
                 #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",URI=\"audio.m3u8\"\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=2000,AUDIO=\"a\"\n\
                 data:application/vnd.apple.mpegurl;base64,AAAA\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=1000,AUDIO=\"a\"\n\
                 video.m3u8\n";
        let s = select(m, &base(), 0).unwrap();
        assert_eq!(s.bitrate, 1000);
        assert_eq!(s.video.scheme(), "https");
        assert!(s.audio.unwrap().as_str().ends_with("/audio.m3u8"));
        let m = m.replace("URI=\"audio.m3u8\"", "URI=\"ftp://x/a.m3u8\"");
        assert_eq!(select(&m, &base(), 0).unwrap().audio, None);
    }

    #[test]
    fn audio_prefers_autoselect_over_listing_order() {
        // the broadpeak master with its audio description listed first
        let ad = "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-aacl-211\",LANGUAGE=\"pol\",NAME=\"Audiodeskrypcja\",AUTOSELECT=NO,CHANNELS=\"2\",URI=\"TVP_Historia_2-audio_ad=211000.m3u8\"\n";
        let m = BROADPEAK
            .replace(ad, "")
            .replace("# AUDIO groups\n", &format!("# AUDIO groups\n{ad}"));
        assert!(m.find("Audiodeskrypcja") < m.find("Polski"));
        let s = select(&m, &base(), 0).unwrap();
        assert!(s.audio.unwrap().as_str().ends_with("audio_pol=211000.m3u8"));
    }

    #[test]
    fn audio_description_only_as_last_resort() {
        let ad = "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"AD\",DEFAULT=YES,AUTOSELECT=YES,CHARACTERISTICS=\"public.accessibility.describes-video\",URI=\"ad.m3u8\"\n";
        let pol = "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Polski\",URI=\"pol.m3u8\"\n";
        let audio = |renditions: &str| {
            let m =
                format!("#EXTM3U\n{renditions}#EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\nv.m3u8\n");
            select(&m, &base(), 0).unwrap().audio.unwrap().to_string()
        };
        assert!(audio(&format!("{ad}{pol}")).ends_with("/pol.m3u8"));
        assert!(audio(ad).ends_with("/ad.m3u8"));
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
