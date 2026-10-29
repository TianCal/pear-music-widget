//! Kugeci (`www.kugeci.com`): timed lyrics for the Chinese catalogue, found
//! by title search and verified against title and artist on both the result
//! row and the song page. The site supplies LRC in `#lyricsContainer`, without
//! duration or album metadata; a failed request or changed markup is a miss.

use std::sync::LazyLock;

use regex::Regex;

use super::{parse_lrc, How, Lyrics, TIMEOUT, USER_AGENT};
use crate::state::Song;
use crate::text::{fold, kugeci_title, urlencode_form};

const KUGECI_ENDPOINT: &str = "https://www.kugeci.com";
const KUGECI_MAX_BYTES: usize = 1024 * 1024;

static KUGECI_SUBTITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?)\s*[（(]([^()（）]+)[）)]\s*$").unwrap());
static KUGECI_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:live|remix|mix|acoustic|instrumental|version|ver|cover|karaoke|edit|demo|part|pt|mono|stereo|remaster(?:ed)?)\b|现场|現場|翻唱|伴奏|混音|版|录音|錄音").unwrap()
});

/// A lyric hook is sometimes appended to a song title. Try the literal title
/// first, then omit a trailing subtitle; recording-version labels stay intact.
fn kugeci_titles(title: &str) -> Vec<String> {
    let full = kugeci_title(title);
    let mut titles = vec![full.clone()];
    if let Some(parts) = KUGECI_SUBTITLE.captures(&full) {
        let base = parts[1].trim();
        if !base.is_empty() && !KUGECI_VERSION.is_match(&parts[2]) {
            titles.push(base.to_string());
        }
    }
    titles
}

/// The keys a site title may match ours under. Computed once per search:
/// every row of the results is checked against the same ones.
fn kugeci_title_keys(title: &str) -> Vec<String> {
    kugeci_titles(title).iter().map(|title| kugeci_key(title)).collect()
}

fn kugeci_title_matches(candidate: &str, title: &str) -> bool {
    kugeci_title_matches_keys(candidate, &kugeci_title_keys(title))
}

fn kugeci_title_matches_keys(candidate: &str, wanted: &[String]) -> bool {
    let candidate = kugeci_key(&kugeci_title(candidate));
    !candidate.is_empty() && wanted.iter().any(|title| *title == candidate)
}

/// `text::fold`, plus 迴→回, which the site's titles use interchangeably.
fn kugeci_key(text: &str) -> String {
    fold(text).replace('迴', "回")
}

static KUGECI_ARTIST_SPLIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)、|,|，|/|&|(?-u:\b)(?:and|with|feat|ft)(?-u:\b)\.?|和|与|與|及").unwrap()
});

fn kugeci_artist_key(artist: &str) -> String {
    let key = kugeci_key(artist);
    // Verified provider alias, not a general prefix/suffix fuzzy match.
    match key.as_str() {
        "华云龙kle" => "华云龙".into(),
        _ => key,
    }
}

/// Match literal artist names first, then segment collaborations using the
/// site's actual names. Splitting blindly on “和” breaks names like 楊和蘇.
fn kugeci_artists_match(credits: &[String], artist: &str) -> bool {
    let keys: Vec<_> = credits
        .iter()
        .map(|credit| kugeci_artist_key(credit))
        .collect();
    let artist_key = kugeci_artist_key(artist);
    if artist_key.is_empty() {
        return false;
    }
    if keys.contains(&artist_key) {
        return true;
    }
    if !KUGECI_ARTIST_SPLIT.is_match(artist) && !artist.chars().any(char::is_whitespace) {
        return false;
    }
    let names: Vec<&str> = keys
        .iter()
        .filter(|key| !key.is_empty())
        .map(String::as_str)
        .collect();
    if names.is_empty() {
        return false;
    }
    // Apply the one verified alias inside a collaboration too.
    let artist_key = artist_key.replace("华云龙kle", "华云龙");
    // The credit must be two or more of the site's names in a row, each
    // after the first optionally introduced by a joiner, and nothing else.
    // Tried every way, as the regex it replaces did, so one name being a
    // prefix of another cannot hide a match; the strings are a few words.
    fn spelled(rest: &str, names: &[&str], count: usize) -> bool {
        const JOINERS: [&str; 8] = ["featuring", "feat", "with", "and", "ft", "和", "与", "及"];
        if rest.is_empty() {
            return count >= 2;
        }
        let after_joiner = (count >= 1)
            .then(|| JOINERS.iter().filter_map(|joiner| rest.strip_prefix(joiner)))
            .into_iter()
            .flatten();
        std::iter::once(rest).chain(after_joiner).any(|from| {
            names
                .iter()
                .filter_map(|name| from.strip_prefix(name))
                .any(|left| spelled(left, names, count + 1))
        })
    }
    spelled(&artist_key, &names, 0)
}

/// The page's selectors, parsed once: a search page is walked row by row
/// and cell by cell, and parsing a selector for each was most of the walk.
struct Selectors {
    rows: scraper::Selector,
    cells: scraper::Selector,
    links: scraper::Selector,
    heading: scraper::Selector,
    credits: scraper::Selector,
    lyrics: scraper::Selector,
}

static SELECT: LazyLock<Selectors> = LazyLock::new(|| {
    let select = |selector| scraper::Selector::parse(selector).expect("static lyrics selector");
    Selectors {
        rows: select("#tablesort tbody tr"),
        cells: select("td"),
        links: select("a"),
        heading: select("main h1"),
        credits: select(".song-details-container a"),
        lyrics: select("#lyricsContainer"),
    }
});

fn kugeci_song_path(href: &str) -> Option<String> {
    let path = href.strip_prefix(KUGECI_ENDPOINT).unwrap_or(href);
    let id = path.strip_prefix("/song/")?;
    (!id.is_empty() && id.bytes().all(|ch| ch.is_ascii_alphanumeric())).then(|| path.to_string())
}

/// Search matches the entire query, so use the title alone, then verify the
/// artist in the same row. Footer recommendations must never become candidates.
fn kugeci_candidates(html: &str, title: &str, artist: &str) -> Vec<String> {
    let document = scraper::Html::parse_document(html);
    let title_key = kugeci_key(&kugeci_title(title));
    let artist_key = kugeci_key(artist);
    if title_key.is_empty() || artist_key.is_empty() {
        return Vec::new();
    }
    let wanted = kugeci_title_keys(title);
    let mut paths = Vec::new();
    for row in document.select(&SELECT.rows) {
        let cells: Vec<_> = row.select(&SELECT.cells).collect();
        let Some(link) = cells
            .get(1)
            .and_then(|cell| cell.select(&SELECT.links).next())
        else {
            continue;
        };
        let name = link.text().collect::<String>();
        if !kugeci_title_matches_keys(&name, &wanted) {
            continue;
        }
        let matches_artist = cells.get(2).is_some_and(|cell| {
            let credits = cell
                .select(&SELECT.links)
                .map(|link| link.text().collect::<String>())
                .collect::<Vec<_>>();
            kugeci_artists_match(&credits, artist)
        });
        if matches_artist {
            if let Some(path) = link.value().attr("href").and_then(kugeci_song_path) {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

fn shape_kugeci(html: &str, title: &str, artist: &str, duration: f64) -> Option<Lyrics> {
    let document = scraper::Html::parse_document(html);
    let name = document
        .select(&SELECT.heading)
        .next()?
        .text()
        .collect::<String>();
    if !kugeci_title_matches(&name, title) {
        return None;
    }
    let credits: Vec<_> = document
        .select(&SELECT.credits)
        .filter(|link| {
            link.value().attr("href").is_some_and(|href| {
                href.strip_prefix(KUGECI_ENDPOINT)
                    .unwrap_or(href)
                    .starts_with("/singer/")
            })
        })
        .map(|link| {
            let label = link.text().collect::<String>();
            label
                .trim()
                .strip_prefix("演唱：")
                .unwrap_or(label.trim())
                .trim()
                .to_string()
        })
        .collect();
    if !kugeci_artists_match(&credits, artist) {
        return None;
    }
    let container = document.select(&SELECT.lyrics).next()?;
    let mut lrc = String::new();
    for node in container.descendants() {
        match node.value() {
            scraper::Node::Text(text) => lrc.push_str(text),
            scraper::Node::Element(element) if element.name() == "br" => lrc.push('\n'),
            _ => {}
        }
    }
    let lines = parse_lrc(&lrc);
    if !lines.iter().any(|line| !line.text.is_empty()) {
        return None;
    }
    // The site provides no recording duration. Reject obvious mismatches, but
    // allow an outro after the last cue; cue time is not track duration.
    if duration > 0.0 && lines.last()?.time? > duration + 10.0 {
        return None;
    }
    Some(Lyrics {
        synced: true,
        lines,
        how: How::Kugeci,
    })
}

async fn kugeci_page(http: &reqwest::Client, path: &str) -> Option<String> {
    let mut response = http
        .get(format!("{KUGECI_ENDPOINT}{path}"))
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(TIMEOUT)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > KUGECI_MAX_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).ok()
}

static KUGECI_VIDEO_CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*([^:：]+?)\s*[:：]\s*[「『]([^」』]+)[」』](.*?)[【\[(（]\s*official\s+(?:mv|music\s+video)\s*[】\])）]\s*$").unwrap()
});

static KUGECI_PLAIN_MV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*([^:：]+?)\s*[:：]\s*(.+?)[【\[]\s*official\s+(?:mv|music\s+video)\s*[】\]](.*)$",
    )
    .unwrap()
});
static KUGECI_LYRIC_VIDEO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(.+?)\s+-\s+([^『「【]+?)(?:[『「]([^』」]*)[』」])?[【\[](?:動態歌詞|动态歌词|動態歌詞字幕|动态歌词字幕)(?:\s*/?\s*lyrics)?[】\]]\s*$").unwrap()
});
// Some uploaders supply explicit performer credits without a video marker.
// Keep this form unadorned: annotated videos must use the stricter parsers.
static KUGECI_PLAIN_CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*([^\[\]【】『』「」()（）]+?)\s+-\s+([^\[\]【】『』「」()（）]+?)\s*$")
        .unwrap()
});
static KUGECI_CCTV_CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^歌曲Top\d+《([^》]+)》\s*(.+?)\s*【\d{4}年央视春晚】(?:｜订阅CCTV春晚)?\s*$")
        .unwrap()
});

fn kugeci_video_credit(title: &str) -> Option<(String, String)> {
    if let Some(parts) = KUGECI_VIDEO_CREDIT.captures(title) {
        if !parts[3].chars().any(|ch| ch.is_alphanumeric()) {
            return Some((parts[2].trim().into(), parts[1].trim().into()));
        }
    }
    if let Some(parts) = KUGECI_PLAIN_MV.captures(title) {
        if parts[2].contains(['「', '『', '」', '』']) {
            return None;
        }
        // Keep title version labels, and reject version/reaction annotations
        // appended after the official marker. Sponsor text is not a credit.
        if !KUGECI_VERSION.is_match(&parts[3]) && !parts[3].to_lowercase().contains("reaction") {
            let title = parts[2].trim_matches(|ch: char| {
                !ch.is_alphanumeric() && !matches!(ch, '(' | ')' | '（' | '）')
            });
            let artist = parts[1].trim_matches(|ch: char| !ch.is_alphanumeric());
            if !title.is_empty() && !artist.is_empty() {
                return Some((title.into(), artist.into()));
            }
        }
    }
    if let Some(parts) = KUGECI_LYRIC_VIDEO.captures(title) {
        if parts
            .get(3)
            .is_some_and(|annotation| KUGECI_VERSION.is_match(annotation.as_str()))
        {
            return None;
        }
        return Some((parts[2].trim().into(), parts[1].trim().into()));
    }
    if let Some(parts) = KUGECI_PLAIN_CREDIT.captures(title) {
        return Some((parts[2].trim().into(), parts[1].trim().into()));
    }
    if let Some(parts) = KUGECI_CCTV_CREDIT.captures(title) {
        return Some((parts[1].trim().into(), parts[2].trim().into()));
    }
    None
}

pub(super) async fn fetch_kugeci_for_song(http: &reqwest::Client, song: &Song) -> Option<Lyrics> {
    tokio::time::timeout(TIMEOUT, async {
        if let Some(words) = fetch_kugeci(http, &song.title, &song.artist, song.song_duration).await
        {
            return Some(words);
        }
        // Music/lyric videos and CCTV clips can report publishers as artists.
        // Use only recognized explicit title/performer credit formats, and
        // verify both fields against the search row and fetched song page.
        let (title, artist) = kugeci_video_credit(&song.title)?;
        fetch_kugeci(http, &title, &artist, song.song_duration).await
    })
    .await
    .ok()
    .flatten()
}

async fn fetch_kugeci(
    http: &reqwest::Client,
    title: &str,
    artist: &str,
    duration: f64,
) -> Option<Lyrics> {
    if kugeci_key(title).is_empty() || kugeci_key(artist).is_empty() {
        return None;
    }
    // Bound the whole fallback, including the search and up to two duplicate
    // recordings, so an unavailable site cannot add several request timeouts.
    tokio::time::timeout(TIMEOUT, async {
        let mut queries = Vec::new();
        for title in kugeci_titles(title) {
            let query = title
                .trim_matches(|ch: char| !ch.is_alphanumeric())
                .to_string();
            if query.is_empty() {
                continue;
            }
            for query in [query.clone(), fast2s::convert(&query).replace('迴', "回")] {
                if !queries.contains(&query) {
                    queries.push(query);
                }
            }
        }
        let mut paths = Vec::new();
        for query in queries {
            if let Some(html) = kugeci_page(http, &format!("/search?q={}", urlencode_form(&query))).await
            {
                paths = kugeci_candidates(&html, title, artist);
                if !paths.is_empty() {
                    break;
                }
            }
        }
        for path in paths.into_iter().take(2) {
            if let Some(html) = kugeci_page(http, &path).await {
                if let Some(words) = shape_kugeci(&html, title, artist, duration) {
                    return Some(words);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic text with the site's actual markup; no copyrighted lyric fixture.
    const KUGECI_SONG: &str = r#"<main><h1> 愛情 </h1>
        <div class="song-details-container"><a href="/writer/x">作词：Someone</a>
        <a href="/singer/a">演唱： Singer &amp; Co</a></div>
        <div id="lyricsContainer">[00:01.20]First &amp; second<br>[00:02.00]<br />[00:03.40]<span>Last</span></div>
        <div id="txt">Duplicated plain lyrics</div></main>"#;

    #[test]
    fn kugeci_parses_entities_breaks_and_instrumental_gaps() {
        let lyrics = shape_kugeci(
            KUGECI_SONG,
            "爱情 (Official Music Video)",
            "Singer & Co",
            4.0,
        )
        .unwrap();
        assert!(lyrics.synced);
        assert_eq!(lyrics.how, How::Kugeci);
        assert_eq!(lyrics.lines.len(), 3);
        assert_eq!(lyrics.lines[0].text, "First & second");
        assert_eq!(lyrics.lines[0].time, Some(1.2));
        assert_eq!(lyrics.lines[1].text, "");
        assert_eq!(lyrics.lines[2].text, "Last");
    }

    #[test]
    fn kugeci_rejects_different_recordings_and_missing_markup() {
        assert!(shape_kugeci(KUGECI_SONG, "爱情 (Live)", "Singer & Co", 4.0).is_none());
        assert!(shape_kugeci(KUGECI_SONG, "爱情", "Someone", 4.0).is_none());
        assert!(shape_kugeci(
            &KUGECI_SONG.replace("[00:03.40]", "[04:00.00]"),
            "爱情",
            "Singer & Co",
            4.0
        )
        .is_none());
        assert!(shape_kugeci("<main>Challenge page</main>", "爱情", "Singer", 4.0).is_none());
        assert!(shape_kugeci(
            &KUGECI_SONG.replace("lyricsContainer", "changed"),
            "爱情",
            "Singer & Co",
            4.0
        )
        .is_none());
    }

    #[test]
    fn kugeci_search_verifies_artist_and_ignores_footer_and_external_links() {
        let html = r#"<table id="tablesort"><tbody>
        <tr><td>date</td><td><a href="/song/studio">爱情</a></td><td><a href="/singer/a">Wrong singer</a></td></tr>
        <tr><td>date</td><td><a href="/song/live">爱情 (Live)</a></td><td><a>Singer &amp; Co</a></td></tr>
        <tr><td>date</td><td><a href="https://www.kugeci.com/song/right">愛情</a></td><td><a>Other</a><a>Singer &amp; Co</a></td><td><a href="/song/right">link</a></td></tr>
        <tr><td>date</td><td><a href="https://evil.test/song/wrong">爱情</a></td><td><a>Singer &amp; Co</a></td></tr>
        </tbody></table><footer><a href="/song/footer">爱情</a></footer>"#;
        assert_eq!(
            kugeci_candidates(html, "爱情", "Singer & Co"),
            vec!["/song/right"]
        );
        assert!(kugeci_candidates(html, "爱情", "").is_empty());
        for path in [
            "//evil.test/song/id",
            "/song/../other",
            "https://www.kugeci.com.evil.test/song/id",
        ] {
            assert!(kugeci_song_path(path).is_none());
        }
    }

    #[test]
    fn kugeci_lyric_video_keeps_recording_annotations() {
        for annotation in ["Live", "Remix", "Acoustic", "粤语版", "现场版"] {
            let title = format!("Singer - Song『{annotation}』【動態歌詞Lyrics】");
            assert!(kugeci_video_credit(&title).is_none(), "{annotation}");
        }
        assert_eq!(
            kugeci_video_credit("Singer - Song『a lyric hook』【動態歌詞Lyrics】"),
            Some(("Song".into(), "Singer".into()))
        );
    }

    #[test]
    fn kugeci_resolves_verified_queue_metadata_formats() {
        assert!(kugeci_artists_match(
            &["Vansdaddy".into(), "华云龙KLE".into()],
            "Vansdaddy和华云龙"
        ));
        let cases = [
            ("🏍C-BLOCK : 很高兴认识你  🛵【 OFFICIAL MV 】Sup Music X 陌陌  \"送给每个美好的相遇\"", "很高兴认识你", "C-BLOCK"),
            ("陳韻若 - 愛的迴歸線『在愛的迴歸線 陽光在手指間』【動態歌詞Lyrics】", "愛的迴歸線", "陳韻若"),
            ("歌曲Top9《答案》杨坤 郭采洁 【2014年央视春晚】｜订阅CCTV春晚", "答案", "杨坤 郭采洁"),
        ];
        for (video_title, title, artist) in cases {
            assert_eq!(
                kugeci_video_credit(video_title),
                Some((title.into(), artist.into()))
            );
        }
        assert!(kugeci_artists_match(
            &["杨坤".into(), "郭采洁".into()],
            "杨坤 郭采洁"
        ));
        assert!(kugeci_title_matches("爱的回归线", "愛的迴歸線"));
        assert!(!kugeci_artists_match(&["华云龙KLE".into()], "华云龙Other"));
        assert!(!kugeci_artists_match(&["A".into(), "B".into()], "AB"));
        assert!(kugeci_artists_match(
            &["楊和蘇KeyNG".into(), "JinJiBeWater_隼".into()],
            "楊和蘇KeyNG和JinJiBeWater_隼"
        ));
        for title in [
            "A : Song 【OFFICIAL MV】 (Live)",
            "A : Song 【OFFICIAL MV】 reaction",
            "A - Song【reaction Lyrics】",
            "歌曲Top9《答案》不同人【2014年其他节目】",
        ] {
            assert!(kugeci_video_credit(title).is_none());
        }
    }

    #[test]
    fn kugeci_extracts_current_san_xin_credit() {
        assert_eq!(
            kugeci_video_credit("KEY.L刘聪 ft. 李佳隆 - 散心"),
            Some(("散心".into(), "KEY.L刘聪 ft. 李佳隆".into()))
        );
    }

    #[test]
    fn kugeci_san_xin_verifies_search_and_page_credits() {
        let (title, artist) = kugeci_video_credit("KEY.L刘聪 ft. 李佳隆 - 散心").unwrap();
        let search = r#"<table id="tablesort"><tbody><tr><td>date</td>
            <td><a href="/song/Cc6WfjTb">散心</a></td>
            <td><a>KEY.L刘聪</a><a>李佳隆</a></td></tr></tbody></table>"#;
        let page = r#"<main><h1>散心</h1><div class="song-details-container">
            <a href="/singer/a">演唱： KEY.L刘聪</a>
            <a href="/singer/b">演唱： 李佳隆</a></div>
            <div id="lyricsContainer">[00:04.851]fixture<br />[00:06.718]line</div></main>"#;
        assert_eq!(
            kugeci_candidates(search, &title, &artist),
            vec!["/song/Cc6WfjTb"]
        );
        assert!(shape_kugeci(page, &title, &artist, 265.0).is_some());
        assert!(shape_kugeci(page, &title, "FirePanda", 265.0).is_none());
        assert!(shape_kugeci(page, &title, "KEY.L刘聪 ft. Someone", 265.0).is_none());
        for annotated in [
            "KEY.L刘聪 ft. 李佳隆 - 散心 (Live)",
            "A - Song【reaction Lyrics】",
        ] {
            assert!(kugeci_video_credit(annotated).is_none());
        }
    }

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_san_xin_uploader() {
        let song = Song {
            title: "KEY.L刘聪 ft. 李佳隆 - 散心".into(),
            artist: "FirePanda".into(),
            song_duration: 265.0,
            ..Song::default()
        };
        let words = fetch_kugeci_for_song(&reqwest::Client::new(), &song)
            .await
            .expect("uploader title resolves to synced Kugeci lyrics");
        assert!(words.synced);
        assert_eq!(words.how, How::Kugeci);
        assert!(words.lines.len() > 20);
    }

    #[test]
    fn kugeci_extracts_explicit_music_video_credit_from_uploader_metadata() {
        let title = "功夫胖 KUNGFU-PEN ：「无赖」🐼 🐼 🐼 【 OFFICIAL MV  】";
        assert_eq!(
            kugeci_video_credit(title),
            Some(("无赖".into(), "功夫胖 KUNGFU-PEN".into()))
        );
        for title in ["A: Song", "A ：「Song」 reaction video", "Song (Live)"] {
            assert!(kugeci_video_credit(title).is_none());
        }
    }

    #[test]
    fn kugeci_matches_alternate_subtitle_without_changing_recording_version() {
        let search = r#"<table id="tablesort"><tbody><tr><td>date</td>
            <td><a href="/song/9QbM6b0K">月半小夜曲</a></td><td><a>TizzyT</a></td></tr></tbody></table>"#;
        for title in ["月半小夜曲 (你怎么不回答)", "月半小夜曲（你怎么不回答）"]
        {
            assert_eq!(
                kugeci_candidates(search, title, "Tizzy T"),
                vec!["/song/9QbM6b0K"]
            );
        }
        for title in [
            "月半小夜曲 (Live)",
            "月半小夜曲（现场版）",
            "月半小夜曲 (Remix)",
        ] {
            assert!(kugeci_candidates(search, title, "Tizzy T").is_empty());
        }
    }

    #[test]
    fn kugeci_matches_zhou_xuan_chinese_collaboration_metadata() {
        let search = r#"<table id="tablesort"><tbody><tr><td>date</td>
            <td><a href="/song/7CJI2dO0">周旋</a></td>
            <td><a>王以太</a><a>艾热 AIR</a></td></tr></tbody></table>"#;
        let artist = "王以太和艾热 AIR";
        assert_eq!(
            kugeci_candidates(search, "周旋", &artist),
            vec!["/song/7CJI2dO0"]
        );
        let page = r#"<main><h1>周旋</h1><div class="song-details-container">
            <a href="/singer/a">演唱： 王以太</a><a href="/singer/b">演唱： 艾热 AIR</a></div>
            <div id="lyricsContainer">[00:21.87]Example<br>[00:25.89]Next</div></main>"#;
        assert!(shape_kugeci(page, "周旋", &artist, 291.0).is_some());
    }

    #[test]
    fn kugeci_requires_all_collaborators_and_preserves_literal_artist_names() {
        let credits = vec!["王以太".to_string(), "艾热 AIR".to_string()];
        for artist in [
            "王以太和艾热 AIR",
            "王以太 & 艾熱 AIR",
            "王以太/艾热 AIR",
            "王以太 feat. 艾热 AIR",
        ] {
            assert!(kugeci_artists_match(&credits, artist), "{artist}");
        }
        assert!(!kugeci_artists_match(&credits, "王以太和其他歌手"));
        assert!(!kugeci_artists_match(&credits[..1], "王以太和艾热 AIR"));
        assert!(kugeci_artists_match(&["和平饭店".into()], "和平饭店"));
        assert!(kugeci_artists_match(&["Singer & Co".into()], "Singer & Co"));
    }

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_verified_queue_formats() {
        let cases = [
            ("台北一夜", "Vansdaddy和华云龙", 196.0),
            ("🏍C-BLOCK : 很高兴认识你  🛵【 OFFICIAL MV 】Sup Music X 陌陌  \"送给每个美好的相遇\"", "ZHONG.TV", 259.0),
            ("陳韻若 - 愛的迴歸線『在愛的迴歸線 陽光在手指間』【動態歌詞Lyrics】", "Music Channel HM", 258.0),
            ("歌曲Top9《答案》杨坤 郭采洁 【2014年央视春晚】｜订阅CCTV春晚", "CCTV春晚", 205.0),
        ];
        let http = reqwest::Client::new();
        for (title, artist, song_duration) in cases {
            let song = Song {
                title: title.into(),
                artist: artist.into(),
                song_duration,
                ..Song::default()
            };
            let words = fetch_kugeci_for_song(&http, &song)
                .await
                .unwrap_or_else(|| panic!("no result for {title}"));
            assert!(words.synced);
            assert_eq!(words.how, How::Kugeci);
            assert!(words.lines.len() > 20);
        }
    }

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_uploader_music_video() {
        let song = Song {
            title: "功夫胖 KUNGFU-PEN ：「无赖」🐼 🐼 🐼 【 OFFICIAL MV  】".into(),
            artist: "ZHONG.TV".into(),
            song_duration: 279.0,
            ..Song::default()
        };
        let words = fetch_kugeci_for_song(&reqwest::Client::new(), &song)
            .await
            .expect("explicit video title credit resolves to Kugeci");
        assert!(words.synced);
        assert_eq!(words.how, How::Kugeci);
    }

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_alternate_subtitle() {
        let words = fetch_kugeci(
            &reqwest::Client::new(),
            "月半小夜曲 (你怎么不回答)",
            "Tizzy T",
            230.0,
        )
        .await
        .expect("alternate title matches the same recording");
        assert!(words.synced);
        assert_eq!(words.how, How::Kugeci);
    }

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_zhou_xuan() {
        let words = fetch_kugeci(&reqwest::Client::new(), "周旋", "王以太和艾热 AIR", 291.0)
            .await
            .expect("current track matches the collaboration and timings");
        assert!(words.synced);
        assert_eq!(words.how, How::Kugeci);
        assert!(words.lines.len() > 20);
    }

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_supplied_song() {
        let words = fetch_kugeci(&reqwest::Client::new(), "月半小夜曲", "TizzyT", 0.0)
            .await
            .expect("supplied song found through title search");
        assert!(words.synced);
        assert_eq!(words.how, How::Kugeci);
        assert!(words.lines.len() > 20);
    }
}
