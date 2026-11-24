//! Plain-text helpers: Markdown cleanup, tokenization, wikilink extraction and
//! snippet generation.
//!
//! These are intentionally dependency-free so the crate stays static-`musl`
//! friendly and cheap to compile.

use std::collections::HashMap;

use crate::embedder::stem_variants;

/// Maximum number of characters in a generated plain-text snippet.
pub(crate) const SNIPPET_MAX_CHARS: usize = 140;

/// Lowercase alphanumeric Unicode words.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .collect()
}

/// Trim, lowercase, and collapse internal whitespace.
pub(crate) fn normalize_name(input: &str) -> String {
    input
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Term frequencies for an already-tokenized body.
pub(crate) fn term_frequencies(tokens: &[String]) -> HashMap<String, u32> {
    let mut frequencies = HashMap::new();
    for token in tokens {
        *frequencies.entry(token.clone()).or_insert(0) += 1;
    }
    frequencies
}

/// Map every morphological stem variant of every token to a frequency sum.
pub(crate) fn stem_frequencies(tokens: &[String]) -> HashMap<String, u32> {
    let mut frequencies: HashMap<String, u32> = HashMap::new();
    for token in tokens {
        for stem in stem_variants(token) {
            *frequencies.entry(stem).or_insert(0) += 1;
        }
    }
    frequencies
}

/// Collect the distinct stem variants of a token list (used by queries).
pub(crate) fn query_stems(tokens: &[String]) -> Vec<String> {
    let mut stems: Vec<String> = Vec::new();
    for token in tokens {
        for stem in stem_variants(token) {
            if !stems.contains(&stem) {
                stems.push(stem);
            }
        }
    }
    stems
}

/// Strip Markdown structure and inline syntax, leaving clean plain text.
pub(crate) fn clean_body(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut fence: Option<char> = None;

    for line in markdown.lines() {
        let trimmed = line.trim_start();

        if let Some(marker) = fence {
            if trimmed.starts_with(&marker.to_string().repeat(3)) {
                fence = None;
            } else {
                push_segment(&mut out, trimmed);
            }
            continue;
        }

        if let Some(marker) = fence_marker(trimmed) {
            fence = Some(marker);
            continue;
        }

        let cleaned = clean_line(line);
        push_segment(&mut out, &cleaned);
    }

    collapse_whitespace(&out)
}

fn push_segment(out: &mut String, segment: &str) {
    let segment = segment.trim();
    if segment.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(segment);
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Returns the fence marker when the line opens/closes a fenced code block.
fn fence_marker(line: &str) -> Option<char> {
    let bytes = line.as_bytes();
    if bytes.len() >= 3 && bytes[0] == bytes[1] && bytes[1] == bytes[2] {
        match bytes[0] {
            b'`' => Some('`'),
            b'~' => Some('~'),
            _ => None,
        }
    } else {
        None
    }
}

fn clean_line(line: &str) -> String {
    let mut text = line.trim();

    // Horizontal rules.
    let non_space = text.chars().filter(|ch| !ch.is_whitespace()).count();
    if non_space >= 3
        && text
            .chars()
            .all(|ch| ch.is_whitespace() || matches!(ch, '-' | '*' | '_'))
    {
        return String::new();
    }

    // Blockquotes.
    while let Some(rest) = text.strip_prefix('>') {
        text = rest.trim_start();
    }

    // ATX headings.
    let hashes = text.chars().take_while(|ch| *ch == '#').count();
    if hashes > 0 {
        let rest = &text[hashes..];
        if rest.is_empty() || rest.starts_with(' ') {
            text = rest.trim_start();
        }
    }

    // List markers and task checkboxes.
    loop {
        let mut changed = false;
        for marker in ["- ", "* ", "+ "] {
            if let Some(rest) = text.strip_prefix(marker) {
                text = rest.trim_start();
                changed = true;
                break;
            }
        }
        if !changed {
            let digit_end = text
                .find(|ch: char| !ch.is_ascii_digit())
                .unwrap_or(text.len());
            if digit_end > 0
                && let Some(rest) = text[digit_end..]
                    .strip_prefix(". ")
                    .or_else(|| text[digit_end..].strip_prefix(") "))
            {
                text = rest.trim_start();
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for checkbox in ["[ ] ", "[x] ", "[X] "] {
        if let Some(rest) = text.strip_prefix(checkbox) {
            text = rest.trim_start();
            break;
        }
    }

    inline_clean(text)
}

/// Inline Markdown cleanup: wikilinks, links, images, code spans, emphasis and
/// simple HTML tags.
fn inline_clean(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'\\' => match input[index + 1..].chars().next() {
                Some(ch) => {
                    out.push(ch);
                    index += 1 + ch.len_utf8();
                }
                None => index += 1,
            },
            b'!' if bytes.get(index + 1) == Some(&b'[') => {
                if let Some((text, end)) = parse_link(input, index + 1) {
                    out.push_str(&inline_clean(&text));
                    index = end;
                } else {
                    index += 1;
                }
            }
            b'[' if bytes.get(index + 1) == Some(&b'[') => {
                if let Some(relative) = input[index + 2..].find("]]") {
                    let inner = &input[index + 2..index + 2 + relative];
                    if !inner.contains("[[") {
                        out.push_str(&wikilink_display(inner));
                        index += 2 + relative + 2;
                        continue;
                    }
                }
                index += 1;
            }
            b'[' => {
                if let Some((text, end)) = parse_link(input, index) {
                    out.push_str(&inline_clean(&text));
                    index = end;
                } else {
                    index += 1;
                }
            }
            b'`' => {
                let run = bytes[index..]
                    .iter()
                    .take_while(|byte| **byte == b'`')
                    .count();
                let content_start = index + run;
                match find_code_closer(input, content_start, run) {
                    Some((content_end, end)) => {
                        out.push_str(&input[content_start..content_end]);
                        index = end;
                    }
                    None => index = content_start,
                }
            }
            b'<' => {
                if let Some(relative) = input[index..].find('>') {
                    let inner = &input[index + 1..index + relative];
                    let looks_like_tag = inner.starts_with('/')
                        || inner
                            .chars()
                            .next()
                            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '!');
                    if looks_like_tag {
                        index += relative + 1;
                        continue;
                    }
                }
                out.push('<');
                index += 1;
            }
            b'*' | b'_' | b'~' => index += 1,
            _ => {
                let ch = input[index..].chars().next().unwrap_or(' ');
                out.push(ch);
                index += ch.len_utf8();
            }
        }
    }

    out
}

/// Parse `[label](target)` starting at `open` (which must be the `[` byte).
fn parse_link(input: &str, open: usize) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    if bytes.get(open) != Some(&b'[') {
        return None;
    }
    let close_relative = input[open + 1..].find(']')?;
    let close = open + 1 + close_relative;
    if bytes.get(close + 1) != Some(&b'(') {
        return None;
    }
    let paren_relative = input[close + 2..].find(')')?;
    let end = close + 2 + paren_relative + 1;
    Some((input[open + 1..close].to_owned(), end))
}

/// Find the matching run of `run` backticks after `start`.
fn find_code_closer(input: &str, start: usize, run: usize) -> Option<(usize, usize)> {
    let bytes = input.as_bytes();
    let mut probe = start;
    while probe < bytes.len() {
        if bytes[probe] == b'`' {
            let close_run = bytes[probe..]
                .iter()
                .take_while(|byte| **byte == b'`')
                .count();
            if close_run == run {
                return Some((probe, probe + close_run));
            }
            probe += close_run;
        } else {
            probe += input[probe..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(1);
        }
    }
    None
}

fn wikilink_display(inner: &str) -> String {
    let inner = inner.trim();
    let (target, display) = match inner.split_once('|') {
        Some((target, display)) => (target.trim(), Some(display.trim())),
        None => (inner, None),
    };
    let target = target.split('#').next().unwrap_or(target).trim();
    match display.filter(|display| !display.is_empty()) {
        Some(display) => display.to_owned(),
        None => target.to_owned(),
    }
}

/// Extract outgoing `[[wikilink]]` targets, skipping fenced code blocks and
/// inline code spans. Targets are normalized for case-insensitive resolution.
pub(crate) fn extract_wikilink_targets(markdown: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut fence: Option<char> = None;

    for line in markdown.lines() {
        let trimmed = line.trim_start();

        if let Some(marker) = fence {
            if trimmed.starts_with(&marker.to_string().repeat(3)) {
                fence = None;
            }
            continue;
        }

        if let Some(marker) = fence_marker(trimmed) {
            fence = Some(marker);
            continue;
        }

        scan_wikilinks(line, &mut targets);
    }

    targets
}

fn scan_wikilinks(line: &str, out: &mut Vec<String>) {
    let bytes = line.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'`' {
            let run = bytes[index..]
                .iter()
                .take_while(|byte| **byte == b'`')
                .count();
            match find_code_closer(line, index + run, run) {
                Some((_, end)) => index = end,
                None => index += run,
            }
            continue;
        }

        if bytes[index] == b'['
            && bytes.get(index + 1) == Some(&b'[')
            && let Some(relative) = line[index + 2..].find("]]")
        {
            let inner = &line[index + 2..index + 2 + relative];
            if !inner.contains("[[")
                && let Some(target) = wikilink_target(inner)
            {
                out.push(target);
            }
            index += 2 + relative + 2;
            continue;
        }

        index += line[index..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }
}

fn wikilink_target(inner: &str) -> Option<String> {
    let target = inner
        .split_once('|')
        .map(|(target, _)| target)
        .unwrap_or(inner);
    let target = target.split('#').next().unwrap_or(target).trim();
    let normalized = normalize_name(target);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Build a plain-text snippet of at most [`SNIPPET_MAX_CHARS`] characters,
/// centred on the earliest word that matches a query token (exactly, by
/// prefix, or by stem). Falls back to the opening of the body.
pub(crate) fn make_snippet(body: &str, tokens: &[String], stems: &[String]) -> String {
    let body = body.trim();
    if body.is_empty() {
        return String::new();
    }

    let chars: Vec<char> = body.chars().collect();
    if chars.len() <= SNIPPET_MAX_CHARS {
        return body.to_owned();
    }

    let center = match_center(&chars, tokens, stems).unwrap_or(0);
    let budget = SNIPPET_MAX_CHARS.saturating_sub(2);
    let mut start = center.saturating_sub(budget / 2);
    if start + budget > chars.len() {
        start = chars.len() - budget;
    }
    let mut end = (start + budget).min(chars.len());

    while start < end && chars[start].is_whitespace() {
        start += 1;
    }
    while end > start && chars[end - 1].is_whitespace() {
        end -= 1;
    }

    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.extend(chars[start..end].iter());
    if end < chars.len() {
        snippet.push('…');
    }
    snippet
}

fn match_center(chars: &[char], tokens: &[String], stems: &[String]) -> Option<usize> {
    if tokens.is_empty() && stems.is_empty() {
        return None;
    }

    let mut best: Option<usize> = None;
    let mut word_start: Option<usize> = None;
    let mut word = String::new();

    for (index, ch) in chars.iter().enumerate() {
        if ch.is_alphanumeric() {
            if word_start.is_none() {
                word_start = Some(index);
            }
            word.push(*ch);
        } else if let Some(start) = word_start.take() {
            record_match(&word, start, tokens, stems, &mut best);
            word.clear();
        }
    }
    if let Some(start) = word_start {
        record_match(&word, start, tokens, stems, &mut best);
    }

    best
}

fn record_match(
    word: &str,
    start: usize,
    tokens: &[String],
    stems: &[String],
    best: &mut Option<usize>,
) {
    if word.is_empty() {
        return;
    }
    let lower = word.to_lowercase();
    let prefix_hit = tokens.iter().any(|token| lower.starts_with(token.as_str()));
    let stem_hit = !stems.is_empty()
        && stem_variants(&lower)
            .iter()
            .any(|stem| stems.iter().any(|query_stem| query_stem == stem));

    if prefix_hit || stem_hit {
        let center = start + word.chars().count() / 2;
        if best.is_none_or(|current| center < current) {
            *best = Some(center);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_lowercases_and_splits_punctuation() {
        assert_eq!(
            tokenize("Hello, World! Rust-y notes."),
            vec!["hello", "world", "rust", "y", "notes"]
        );
        assert!(tokenize("   ").is_empty());
    }

    #[test]
    fn clean_body_strips_markdown_and_wikilinks() {
        let cleaned = clean_body(
            "# Heading\n\nSome **bold** and _italic_ with [[Other Note|a friendly label]] and `code`.",
        );
        assert!(cleaned.contains("Some bold and italic with a friendly label and code."));
        assert!(!cleaned.contains("[["));
        assert!(!cleaned.contains("**"));
        assert!(!cleaned.contains('#'));
    }

    #[test]
    fn clean_body_handles_lists_and_links() {
        let cleaned =
            clean_body("- [x] task one\n1. see [the docs](https://example.com)\n> quoted");
        assert!(cleaned.contains("task one"));
        assert!(cleaned.contains("see the docs"));
        assert!(cleaned.contains("quoted"));
    }

    #[test]
    fn wikilink_targets_skip_code_blocks() {
        let targets = extract_wikilink_targets(
            "See [[Alpha]] and [[Beta#Section|label]].\n```\n[[Ignored]]\n```\n`[[AlsoIgnored]]`",
        );
        assert_eq!(targets, vec!["alpha".to_owned(), "beta".to_owned()]);
    }

    #[test]
    fn snippets_center_on_matches_and_stay_short() {
        let body = format!("{}needle{}", "alpha ".repeat(80), " omega".repeat(80));
        let snippet = make_snippet(&body, &["needle".to_owned()], &[]);
        assert!(snippet.contains("needle"));
        assert!(snippet.starts_with('…'));
        assert!(snippet.ends_with('…'));
        assert!(snippet.chars().count() <= SNIPPET_MAX_CHARS);
    }

    #[test]
    fn snippets_fall_back_to_opening_without_matches() {
        let body = format!("{} body text", "opening ".repeat(40));
        let snippet = make_snippet(&body, &["missing".to_owned()], &[]);
        assert!(snippet.starts_with("opening"));
        assert!(snippet.chars().count() <= SNIPPET_MAX_CHARS);
    }

    #[test]
    fn snippets_match_morphological_stems() {
        let body = format!(
            "{} sliding panes here {}",
            "filler ".repeat(40),
            "trailer ".repeat(40)
        );
        let tokens = vec!["slides".to_owned()];
        let stems = query_stems(&tokens);
        let snippet = make_snippet(&body, &tokens, &stems);
        assert!(snippet.contains("sliding panes"));
    }
}
