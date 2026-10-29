//! Where the chorus starts, from synced lyrics.
//!
//! A chorus is the block of lines a song keeps coming back to. Find every run
//! of two or more consecutive lines that recurs, score each by how often it
//! recurs (heavily) and how long it is (capped at four lines, or a whole
//! repeated verse-plus-chorus outscores the chorus alone), and prefer one that
//! starts between 20% and 60% of the song. Then refine: a line inside the
//! block (or just before it) that sings the title is where the chorus begins;
//! failing that, a very long block skips its first third, which is the verse
//! that was repeated along with it.
//!
//! Without synced lyrics, or with no repetition in them, start at 35%.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::text::{before_brackets, fold as norm};
use crate::lyrics::Lyrics;

/// Clip start sits this far before the chorus's first line, so the lead-in
/// into it is heard.
const LEAD_IN: f64 = 1.0;
const FALLBACK_AT: f64 = 0.35;

/// How a chorus was found. The renderer labels each (`HOW` in sorter/app.js).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum How {
    /// From the repeated block.
    Lyrics,
    /// Refined to the line that sings the title.
    Title,
    /// Refined a third of the way into a long block.
    Third,
    /// No synced lyrics: a fixed share of the song.
    Fallback,
}

#[derive(Clone, Debug, Serialize)]
pub struct Chorus {
    pub start: f64,
    pub how: How,
    /// The first chorus line, when lyrics found it.
    pub line: Option<String>,
    pub repeats: usize,
}

static CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(作词|作曲|编曲|制作|監製|监制|演唱|词|曲|op|sp|mix|录音|和声|吉他|贝斯|鼓)\s*[:：]",
    )
    .expect("credit regex")
});

/// Repeats are rarely identical in LRC files: 了/的 swaps, a dropped word.
/// Compared on the lines' prepared `chars`, so the n² comparisons below
/// allocate nothing.
fn similar(a: &Line, b: &Line) -> bool {
    if a.key == b.key {
        return true;
    }
    let (la, lb) = (a.len, b.len);
    if la.abs_diff(lb) > 1 || la.min(lb) < 4 {
        return false;
    }
    // Both sorted and deduplicated: the shared characters by one merge pass.
    let (mut i, mut j, mut shared) = (0, 0, 0);
    while i < a.chars.len() && j < b.chars.len() {
        match a.chars[i].cmp(&b.chars[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    shared as f64 / a.chars.len().max(b.chars.len()) as f64 >= 0.85
}

struct Line {
    time: f64,
    text: String,
    key: String,
    /// `key`'s length in characters.
    len: usize,
    /// `key`'s distinct characters, sorted, for `similar`.
    chars: Vec<char>,
}

fn lines_of(lyrics: &Lyrics) -> Vec<Line> {
    lyrics
        .lines
        .iter()
        .filter_map(|line| {
            let time = line.time?;
            let text = line.text.trim();
            if text.is_empty() || CREDIT.is_match(text) || (time < 15.0 && text.contains(" - ")) {
                return None;
            }
            let key = norm(text);
            let len = key.chars().count();
            let mut chars: Vec<char> = key.chars().collect();
            chars.sort_unstable();
            chars.dedup();
            (len >= 2).then(|| Line {
                time,
                text: text.to_string(),
                key,
                len,
                chars,
            })
        })
        .collect()
}

fn refine(lines: &[Line], start: usize, len: usize, title: &str) -> (usize, How) {
    let title = norm(before_brackets(title));
    if title.chars().count() >= 2 {
        let from = start.saturating_sub(6);
        let to = (start + len).min(lines.len());
        if let Some(i) = (from..to).find(|&i| lines[i].key.contains(&title)) {
            return (i, How::Title);
        }
    }
    if len > 8 {
        return (start + len / 3, How::Third);
    }
    (start, How::Lyrics)
}

pub fn find(lyrics: Option<&Lyrics>, title: &str, duration: f64) -> Chorus {
    let fallback = Chorus {
        start: (duration * FALLBACK_AT).max(0.0),
        how: How::Fallback,
        line: None,
        repeats: 0,
    };
    let Some(lyrics) = lyrics.filter(|l| l.synced) else {
        return fallback;
    };
    if duration <= 0.0 {
        return fallback;
    }
    let lines = lines_of(lyrics);
    let n = lines.len();

    // (score, repeats, len, start)
    let mut best: Option<(i64, usize, usize, usize)> = None;
    for i in 0..n {
        for j in i + 1..n {
            let mut len = 0;
            while j + len < n && i + len < j && similar(&lines[i + len], &lines[j + len]) {
                len += 1;
            }
            if len < 2 {
                continue;
            }
            let block = &lines[i..i + len];
            let mut starts: Vec<usize> = Vec::new();
            for s in 0..=n - len {
                let here = (0..len).all(|m| similar(&lines[s + m], &block[m]));
                if here && starts.last().is_none_or(|&last| s >= last + len) {
                    starts.push(s);
                }
            }
            let first = starts[0];
            let at = lines[first].time / duration;
            let mut score = starts.len() as i64 * 3 + len.min(4) as i64;
            if at < 0.15 {
                score -= 4;
            } else if (0.2..=0.6).contains(&at) {
                score += 2;
            }
            let candidate = (score, starts.len(), len, first);
            let better = match best {
                None => true,
                Some(held) => {
                    (candidate.0, candidate.1, candidate.2) > (held.0, held.1, held.2)
                        || ((candidate.0, candidate.1, candidate.2) == (held.0, held.1, held.2)
                            && candidate.3 < held.3)
                }
            };
            if better {
                best = Some(candidate);
            }
        }
    }
    let Some((_, repeats, len, first)) = best else {
        return fallback;
    };
    let (at, how) = refine(&lines, first, len, title);
    Chorus {
        start: (lines[at].time - LEAD_IN).max(0.0),
        how,
        line: Some(lines[at].text.clone()),
        repeats,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::LyricLine;

    fn lrc(lines: &[(f64, &str)]) -> Lyrics {
        Lyrics {
            synced: true,
            how: crate::lyrics::How::Exact,
            lines: lines
                .iter()
                .map(|(time, text)| LyricLine {
                    time: Some(*time),
                    text: text.to_string(),
                    jyutping: None,
                })
                .collect(),
        }
    }

    #[test]
    fn the_title_line_wins_inside_a_repeated_verse() {
        // 明明就: verse and chorus repeat together, the chorus tail repeats once more.
        let words = lrc(&[
            (19.0, "糖果罐裡好多顏色"),
            (24.0, "微笑卻不甜了"),
            (56.0, "我只在意有你的消息"),
            (60.0, "城堡為愛守著秘密"),
            (68.0, "明明就不習慣牽手"),
            (72.0, "為何卻主動把手勾"),
            (85.0, "明明就他比較溫柔"),
            (90.0, "也許他能給你更多"),
            (95.0, "不用抉擇 我會自動變朋友"),
            (117.0, "糖果罐裡好多顏色"),
            (121.0, "微笑卻不甜了"),
            (154.0, "我只在意有你的消息"),
            (158.0, "城堡為愛守著秘密"),
            (165.0, "明明就不習慣牽手"),
            (170.0, "為何卻主動把手勾"),
            (183.0, "明明就他比較溫柔"),
            (187.0, "也許他能給你更多"),
            (193.0, "不用抉擇 我會自動變朋友"),
            (218.0, "他比較溫柔"),
            (222.0, "也許他能給你更多"),
            (226.0, "不用抉擇 我會自動變朋友"),
        ]);
        let found = find(Some(&words), "明明就", 260.0);
        assert_eq!(found.how, How::Title);
        assert_eq!(found.start, 67.0);
    }

    #[test]
    fn no_synced_lyrics_falls_back_to_a_third_of_the_way_in() {
        let found = find(None, "x", 200.0);
        assert_eq!(found.how, How::Fallback);
        assert_eq!(found.start, 70.0);
    }
}
