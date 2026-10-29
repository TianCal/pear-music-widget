//! Folding, bracket stripping and credit parsing, shared by everything that
//! compares names from different sources: artwork providers, lyrics providers,
//! search results, the chorus finder and Sort Songs. They have to agree on
//! what "the same name" is, so the rules live here once.
//!
//! The bracket strippers are deliberately several named rules, not one with
//! options: each matches what its provider will and will not match on, and
//! Kugeci's in particular keeps the notes the others drop.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

// ------------------------------------------------------------------- folding

/// Simplified, lowercased, letters and digits only.
pub fn fold(text: &str) -> String {
    fast2s::convert(text)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Han characters: the catalogue Kugeci covers.
pub fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
    })
}

// ------------------------------------------------------------------ brackets

static BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[(（\[【][^)）\]】]*[)）\]】]").expect("bracket regex"));

/// Every bracketed note, in Western or fullwidth brackets, replaced by a space:
/// artwork providers' release notes, remaster tags, bitrates.
pub fn strip_brackets(text: &str) -> Cow<'_, str> {
    BRACKETS.replace_all(text, " ")
}

/// The title up to its first opening bracket. 小酒窩, 小酒窝 (Live) and
/// 小酒窝(feat.蔡卓妍)（粤语版） all give 小酒窝.
pub fn before_brackets(title: &str) -> &str {
    title.split(['(', '（', '[', '【']).next().unwrap_or(title)
}

static CLEAN_PATTERNS: LazyLock<[Regex; 5]> = LazyLock::new(|| {
    [
        Regex::new(r"\([^)]*\)").unwrap(),
        Regex::new(r"\[[^\]]*\]").unwrap(),
        Regex::new(r"《[^》]*》").unwrap(),
        Regex::new(r"【[^】]*】").unwrap(),
        Regex::new(r"(?i)\b(official|music\s+video|lyrics?|audio|m/?v|hd|4k)\b").unwrap(),
    ]
});

/// YouTube Music titles carry a lot that LRCLib will not match on — bracketed
/// soundtrack credits, 《…》 wrappers, "Official Music Video". Strip them for the
/// fallback search, but try the untouched title first: for plenty of tracks the
/// full string is the exact match.
pub fn clean_title(title: &str) -> String {
    let mut stripped = title.to_string();
    for pattern in CLEAN_PATTERNS.iter() {
        stripped = pattern.replace_all(&stripped, " ").into_owned();
    }
    let stripped = WHITESPACE.replace_all(&stripped, " ").trim().to_string();
    if stripped.is_empty() {
        title.to_string()
    } else {
        stripped
    }
}

// Strip promotional labels only. Keeping Live / remix / acoustic qualifiers
// prevents a studio recording from borrowing timings from a different version.
static KUGECI_PROMO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:official(?:\s+music)?\s+video|official\s+audio|official\s+lyrics?|music\s+video|lyrics?\s+video|official|audio|lyrics?|m/?v|hd|4k)\b").unwrap()
});

// Empty brackets left by promotional labels are immaterial to matching.
static KUGECI_EMPTY_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(\s*\)|\[\s*\]|【\s*】").unwrap());

/// Kugeci's title rule, which is not `clean_title`: only promotional labels
/// go, and the brackets they leave empty. A bracketed Live, remix or 粤语版
/// stays, because the site lists those as their own songs with their own
/// timings.
pub fn kugeci_title(title: &str) -> String {
    let title = KUGECI_PROMO.replace_all(title, " ");
    let title = KUGECI_EMPTY_BRACKETS.replace_all(&title, " ");
    WHITESPACE.replace_all(&title, " ").trim().to_string()
}

// ------------------------------------------------------------------- credits

/// Separators are whole words, but against ASCII letters only (`(?-u:\b)`): a
/// Han character next to "feat." is a boundary, so "陈奕迅feat.王菲" splits.
static ARTIST_SPLIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)、|,|&|(?-u:\b)(?:and|with|feat|ft)(?-u:\b)\.?").unwrap()
});

/// Collaborations are listed several ways; LRCLib matches on the lead artist.
pub fn lead_artist(artist: &str) -> String {
    let head = ARTIST_SPLIT.split(artist).next().unwrap_or("");
    WHITESPACE.replace_all(head, " ").trim().to_string()
}

/// Credit separators YouTube Music and common tag writers use. Not the same
/// set as `lead_artist`'s: this one decides what `artistNames` can rename,
/// the other what LRCLib is asked for, and each is tested on its own.
pub(crate) static CREDIT_SEPARATORS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*(?:,|，|&|、|/|;|(?-u:\b)feat(?-u:\b)\.?|(?-u:\b)ft\.)\s*").expect("separator regex")
});

/// The names in a credit, split where `store::map_artist` splits it.
pub fn split_credit(credit: &str) -> Vec<&str> {
    CREDIT_SEPARATORS
        .split(credit)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

// ---------------------------------------------------------------------- urls

/// Minimal percent-encoding for a path segment or query value: the API
/// server's client id and token.
pub fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `urlencode` as a query string's field: a space is `+`. LRCLib's and
/// Kugeci's searches are asked this way.
pub fn urlencode_form(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_what_lrclib_will_not_match_on() {
        assert_eq!(clean_title("Song (Official Music Video)"), "Song");
        assert_eq!(clean_title("《Theme》 Track [4K]"), "Track");
        // Stripping everything leaves the original rather than an empty query.
        assert_eq!(clean_title("(Official)"), "(Official)");
    }

    #[test]
    fn takes_the_lead_artist_only() {
        assert_eq!(lead_artist("A feat. B"), "A");
        assert_eq!(lead_artist("A、B"), "A");
        assert_eq!(lead_artist("A & B"), "A");
        assert_eq!(lead_artist("Solo"), "Solo");
        assert_eq!(lead_artist("A ft. B"), "A");
        assert_eq!(lead_artist("A with B"), "A");
        // Separators are whole words, not letters inside a name.
        assert_eq!(lead_artist("Taylor Swift"), "Taylor Swift");
        assert_eq!(lead_artist("Bill Withers"), "Bill Withers");
        assert_eq!(lead_artist("Daft Punk"), "Daft Punk");
        assert_eq!(lead_artist("Feather Lane"), "Feather Lane");
        // Glued to Han characters, as Chinese credits often are.
        assert_eq!(lead_artist("陈奕迅feat.王菲"), "陈奕迅");
        assert_eq!(lead_artist("周杰倫ft.費玉清"), "周杰倫");
    }

    #[test]
    fn kugeci_is_for_han_text_only() {
        assert!(has_cjk("不懂"));
        assert!(has_cjk("凍結"));
        assert!(!has_cjk("Always Online"));
        assert!(!has_cjk("Jay"));
    }
}
