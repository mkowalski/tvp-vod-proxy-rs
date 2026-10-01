//! M3U playlist generation.

use crate::tvp::LiveItem;

/// Logo URL from the API's `images` object (`logo`, else first `16x9`).
pub fn logo(item: &LiveItem) -> Option<String> {
    ["logo", "16x9"].iter().find_map(|k| {
        let url = item.images.get(*k)?.get(0)?.get("url")?.as_str()?;
        let url: String = url
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .replace('"', "%22");
        Some(if url.starts_with("//") {
            format!("https:{url}")
        } else {
            url
        })
    })
}

/// Channel title safe for an `#EXTINF` line: commas and control characters
/// (which would split the entry or the line) become spaces.
fn title(raw: &str) -> String {
    raw.chars()
        .map(|c| if c == ',' || c.is_control() { ' ' } else { c })
        .collect()
}

/// One `#EXTINF` + URL pair per playable channel, pointing at the proxy.
pub fn render<'a>(host: &str, items: impl IntoIterator<Item = &'a LiveItem>) -> String {
    let mut out = String::from("#EXTM3U\n");
    for item in items {
        let title = title(&item.title);
        let logo = logo(item)
            .map(|l| format!(" tvg-logo=\"{l}\""))
            .unwrap_or_default();
        out.push_str(&format!(
            "#EXTINF:-1{logo} group-title=\"TVP\",{title}\nhttp://{host}/tvp/{}.ts\n",
            item.id
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(id: u64, title: &str, images: serde_json::Value) -> LiveItem {
        LiveItem {
            id,
            title: title.into(),
            payable: false,
            images,
        }
    }

    #[test]
    fn renders_entries() {
        let items = [
            item(
                399700,
                "TVP KULTURA",
                json!({"16x9": [{"url": "//s.tvp.pl/k.png"}]}),
            ),
            item(399723, "TVP POLONIA", json!({})),
        ];
        let m = render("192.0.2.10:38099", &items);
        assert_eq!(
            m,
            "#EXTM3U\n\
             #EXTINF:-1 tvg-logo=\"https://s.tvp.pl/k.png\" group-title=\"TVP\",TVP KULTURA\n\
             http://192.0.2.10:38099/tvp/399700.ts\n\
             #EXTINF:-1 group-title=\"TVP\",TVP POLONIA\n\
             http://192.0.2.10:38099/tvp/399723.ts\n"
        );
    }

    #[test]
    fn commas_in_title_do_not_break_extinf() {
        let m = render("h:1", &[item(1, "A, B", json!(null))]);
        assert!(m.contains(",A  B\n"));
    }

    #[test]
    fn control_characters_do_not_break_playlist() {
        let i = item(
            1,
            "A\r\nhttp://evil/\tB",
            json!({"logo": [{"url": "https://a/x\".png\n"}]}),
        );
        let m = render("h:1", &[i]);
        assert_eq!(m.lines().count(), 3, "{m}");
        assert!(m.contains(",A  http://evil/ B\n"), "{m}");
        assert!(m.contains("tvg-logo=\"https://a/x%22.png\""), "{m}");
    }

    #[test]
    fn prefers_logo_over_16x9() {
        let i = item(
            1,
            "x",
            json!({"16x9": [{"url": "https://a/16x9.png"}], "logo": [{"url": "https://a/logo.png"}]}),
        );
        assert_eq!(logo(&i).as_deref(), Some("https://a/logo.png"));
    }
}
