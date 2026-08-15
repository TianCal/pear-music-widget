//! Offline Cantonese romanisation for displayed lyrics.
//!
//! The dictionaries ship with the app. This module never performs network I/O;
//! it only turns the original lyric text into a second, display-only line.

use std::collections::HashMap;
use std::path::Path;

use canto_g2p::Pipeline;
use tauri::{path::BaseDirectory, AppHandle, Manager};

pub struct Jyutping {
    pipeline: Option<Pipeline>,
}

impl Jyutping {
    pub fn load(app: &AppHandle) -> Self {
        let dir = app
            .path()
            .resolve("resources/jyutping", BaseDirectory::Resource);
        match dir {
            Ok(dir) => Self::from_dir(&dir),
            Err(error) => {
                eprintln!("jyutping resources unavailable: {error}");
                Self { pipeline: None }
            }
        }
    }

    pub(crate) fn from_dir(dir: &Path) -> Self {
        match Pipeline::from_dir_opts(dir, true) {
            Ok(pipeline) => Self {
                pipeline: Some(pipeline),
            },
            Err(error) => {
                eprintln!("jyutping dictionaries failed to load from {dir:?}: {error}");
                Self { pipeline: None }
            }
        }
    }

    /// Return a readable, numberless Jyutping line when the input contains at
    /// least one Cantonese token. English-only lines are left alone instead of
    /// being duplicated under themselves.
    pub fn annotate(&self, text: &str) -> Option<String> {
        let pipeline = self.pipeline.as_ref()?;
        // The upstream normalizer reads bare digits aloud. Protect complete
        // ASCII words first so an original `AB12` stays `AB12`; only tone
        // digits belonging to generated Cantonese syllables are removed.
        let (protected_text, protected) = protect_ascii_words(text);
        let detailed = pipeline.convert_detailed(&protected_text);
        if !detailed.iter().any(|token| token.2 == "yue") {
            return None;
        }

        let mut output = String::new();
        let mut after_opening = false;
        for (token, reading, language, _, _) in detailed {
            let protected_word = token
                .chars()
                .next()
                .filter(|_| token.chars().count() == 1)
                .and_then(|key| protected.get(&key));
            let piece = if let Some(original) = protected_word {
                original.clone()
            } else if language == "yue" {
                reading
                    .chars()
                    .filter(|ch| !matches!(ch, '1'..='6'))
                    .collect::<String>()
            } else {
                token
            };
            if piece.is_empty() {
                continue;
            }

            let closing = protected_word.is_none() && is_closing_punctuation(&piece);
            let opening = protected_word.is_none() && is_opening_punctuation(&piece);
            if !output.is_empty() && !closing && !after_opening {
                output.push(' ');
            }
            output.push_str(&piece);
            after_opening = opening;
        }

        let output = output.trim().to_string();
        (!output.is_empty()).then_some(output)
    }
}

/// Replace ASCII alphanumeric runs with private-use scalar values that the G2P
/// normalizer passes through. The mapping is local to one lyric line.
fn protect_ascii_words(text: &str) -> (String, HashMap<char, String>) {
    let mut output = String::with_capacity(text.len());
    let mut protected = HashMap::new();
    let mut run = String::new();
    let mut next = 0xE000u32;

    let flush = |output: &mut String,
                 protected: &mut HashMap<char, String>,
                 run: &mut String,
                 next: &mut u32| {
        if run.is_empty() {
            return;
        }
        if let Some(marker) = char::from_u32(*next) {
            output.push(marker);
            protected.insert(marker, std::mem::take(run));
            *next += 1;
        } else {
            output.push_str(run);
            run.clear();
        }
    };

    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            run.push(ch);
        } else {
            flush(&mut output, &mut protected, &mut run, &mut next);
            output.push(ch);
        }
    }
    flush(&mut output, &mut protected, &mut run, &mut next);
    (output, protected)
}

fn is_closing_punctuation(text: &str) -> bool {
    text.chars().all(|ch| {
        matches!(
            ch,
            ',' | '.'
                | '!'
                | '?'
                | ';'
                | ':'
                | '%'
                | ')'
                | ']'
                | '}'
                | '\''
                | '\u{2019}'
                | '\u{201d}'
                | '\u{3001}'
                | '\u{3002}'
                | '\u{ff0c}'
                | '\u{ff01}'
                | '\u{ff1f}'
                | '\u{ff1b}'
                | '\u{ff1a}'
                | '\u{ff05}'
                | '\u{ff09}'
                | '\u{3009}'
                | '\u{300b}'
                | '\u{300d}'
                | '\u{300f}'
                | '\u{3011}'
                | '\u{2026}'
        )
    })
}

fn is_opening_punctuation(text: &str) -> bool {
    text.chars().all(|ch| {
        matches!(
            ch,
            '(' | '['
                | '{'
                | '\u{2018}'
                | '\u{201c}'
                | '\u{ff08}'
                | '\u{3008}'
                | '\u{300a}'
                | '\u{300c}'
                | '\u{300e}'
                | '\u{3010}'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Jyutping {
        Jyutping::from_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/jyutping"))
    }

    #[test]
    fn converts_the_reference_lyrics_without_tone_numbers() {
        let engine = engine();
        assert_eq!(
            engine.annotate("難道從後腦可以望穿我心裏痛甚麼？").as_deref(),
            Some("naan dou cung hau nou ho ji mong cyun ngo sam leoi tung sam mo？")
        );
        assert_eq!(
            engine.annotate("正面像全裸").as_deref(),
            Some("zing min zoeng cyun lo")
        );
        assert_eq!(
            engine.annotate("遮醜的布就別來丟破").as_deref(),
            Some("ze cau dik bou zau bit loi diu po")
        );
    }

    #[test]
    fn does_not_duplicate_english_only_lines() {
        let engine = engine();
        assert_eq!(engine.annotate("Baby, one more time"), None);
    }

    #[test]
    fn preserves_english_and_original_digits_in_mixed_tokens() {
        let engine = engine();
        let result = engine.annotate("我用 AB12 寫歌").expect("annotation");
        assert_eq!(result, "ngo jung AB12 se go");
        assert_eq!(result.chars().filter(|ch| ch.is_ascii_digit()).collect::<String>(), "12");
    }
}
