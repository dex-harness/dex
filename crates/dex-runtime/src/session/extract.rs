//! Turning model output into a program.
//!
//! The model is asked for one program, but it is a language model and it
//! sometimes adds a sentence of explanation first or last. Extraction is
//! therefore forgiving in a defined order rather than strict in one way: a
//! fenced block is preferred, a sentinel is honoured if present, and the whole
//! message is the last resort. Whatever is taken is then handed to the
//! compiler, so a wrong guess costs a compile error the model can read rather
//! than a silent misbehaviour.

/// How a program was located in the model's reply. Reported in diagnostics so a
/// repeated failure is explainable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extraction {
    /// A fenced code block, optionally tagged `rune` or `rust`.
    Fenced { tagged: bool },
    /// An explicit sentinel pair.
    Sentinel,
    /// The entire message, used as-is.
    WholeMessage,
}

/// Extract the program, or `None` when the reply contains nothing runnable.
pub fn extract(reply: &str) -> Option<(String, Extraction)> {
    if let Some((code, tagged)) = fenced(reply) {
        if !code.trim().is_empty() {
            return Some((code, Extraction::Fenced { tagged }));
        }
    }
    if let Some(code) = sentinel(reply) {
        if !code.trim().is_empty() {
            return Some((code, Extraction::Sentinel));
        }
    }
    // A reply that is entirely prose is not a program. Without this the
    // compiler would be asked to parse an explanation, and the resulting
    // syntax error would point at the model's prose rather than at anything it
    // did wrong.
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return None;
    }
    if looks_like_prose(trimmed) {
        return None;
    }
    // Fences that held nothing still appear in a fallback reply; leaving their
    // markers in would hand the compiler punctuation it did not write as code.
    let cleaned = strip_empty_fences(trimmed);
    Some((cleaned, Extraction::WholeMessage))
}

/// Remove fence markers that wrap nothing.
///
/// Only applied on the fallback path, where the reply was not a clean program
/// but still contained one after an empty block.
fn strip_empty_fences(text: &str) -> String {
    if !text.contains("```") {
        return text.to_string();
    }
    let mut out: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            inside = !inside;
            continue;
        }
        if !inside {
            out.push(line);
        }
    }
    out.join("\n").trim().to_string()
}

/// The last fenced block, which is where a corrected program ends up when a
/// model explains what it changed and then shows the new version.
fn fenced(text: &str) -> Option<(String, bool)> {
    let mut found: Option<(String, bool)> = None;
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let newline = after.find('\n')?;
        let info = after[..newline].trim();
        let body_start = newline + 1;
        let body = match after[body_start..].find("```") {
            Some(end) => &after[body_start..body_start + end],
            None => {
                // An unterminated fence is still the intended content; taking
                // the remainder beats rejecting a correct program over a
                // missing closing fence.
                &after[body_start..]
            }
        };
        let tagged = !info.is_empty();
        if !body.trim().is_empty() {
            found = Some((body.trim().to_string(), tagged));
        }
        rest = match after[body_start..].find("```") {
            Some(end) => &after[body_start + end + 3..],
            None => "",
        };
    }
    found
}

fn sentinel(text: &str) -> Option<String> {
    const OPEN: &str = "<dex-program>";
    const CLOSE: &str = "</dex-program>";
    let start = text.find(OPEN)? + OPEN.len();
    let end = text[start..].find(CLOSE)? + start;
    Some(text[start..end].trim().to_string())
}

/// Heuristic for "this is an explanation, not code".
///
/// Deliberately conservative: a reply only counts as prose when it has no line
/// that could begin a program. Under-reading here costs a compile error, while
/// over-reading would discard a valid program.
fn looks_like_prose(text: &str) -> bool {
    let mut has_statement = false;
    let mut has_assignment = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        if line.starts_with("pub fn")
            || line.starts_with("fn ")
            || line.starts_with("let ")
            || line.starts_with("const ")
            || line.starts_with("import ")
            || line.starts_with("use ")
        {
            has_statement = true;
        }
        // `identifier = expression` at the start of a line is code, not prose.
        if let Some((head, _)) = line.split_once('=') {
            let head = head.trim();
            if !head.is_empty()
                && head
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
                && head.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
            {
                has_assignment = true;
            }
        }
    }
    !(has_statement || has_assignment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fenced_rune_block_is_preferred() {
        let reply = "Here you go:\n\n```rune\npub fn main() { 1 }\n```\n\nHope that helps.";
        let (code, how) = extract(reply).expect("extracted");
        assert_eq!(code, "pub fn main() { 1 }");
        assert_eq!(how, Extraction::Fenced { tagged: true });
    }

    #[test]
    fn an_untagged_fence_still_works() {
        let reply = "```\npub fn main() { 2 }\n```";
        let (code, how) = extract(reply).expect("extracted");
        assert_eq!(code, "pub fn main() { 2 }");
        assert_eq!(how, Extraction::Fenced { tagged: false });
    }

    #[test]
    fn a_fence_tagged_with_rust_is_accepted() {
        // Models routinely tag a Rune fence as `rust` out of habit.
        let reply = "```rust\npub fn main() { 3 }\n```";
        let (code, _) = extract(reply).expect("extracted");
        assert_eq!(code, "pub fn main() { 3 }");
    }

    #[test]
    fn the_last_fence_wins_when_a_model_shows_two() {
        // A model that explains and then shows a corrected version puts the
        // version it wants to run last.
        let reply = "First attempt:\n```rune\npub fn main() { 1 }\n```\nCorrected:\n```rune\npub fn main() { 2 }\n```";
        let (code, _) = extract(reply).expect("extracted");
        assert_eq!(code, "pub fn main() { 2 }");
    }

    #[test]
    fn an_unterminated_fence_still_yields_its_content() {
        let reply = "```rune\npub fn main() { 4 }";
        let (code, _) = extract(reply).expect("extracted");
        assert_eq!(code, "pub fn main() { 4 }");
    }

    #[test]
    fn a_sentinel_wins_when_there_is_no_fence() {
        let reply = "Thinking about it.\n<dex-program>\npub fn main() { 5 }\n</dex-program>";
        let (code, how) = extract(reply).expect("extracted");
        assert_eq!(code, "pub fn main() { 5 }");
        assert_eq!(how, Extraction::Sentinel);
    }

    #[test]
    fn a_fence_is_preferred_over_a_sentinel() {
        let reply = "<dex-program>ignored</dex-program>\n```rune\npub fn main() { 6 }\n```";
        let (code, how) = extract(reply).expect("extracted");
        assert_eq!(code, "pub fn main() { 6 }");
        assert!(matches!(how, Extraction::Fenced { .. }));
    }

    #[test]
    fn a_bare_program_is_used_whole() {
        let (code, how) = extract("pub fn main() { 7 }").expect("extracted");
        assert_eq!(code, "pub fn main() { 7 }");
        assert_eq!(how, Extraction::WholeMessage);
    }

    #[test]
    fn a_multiline_bare_program_is_used_whole() {
        let reply = "pub fn main() {\n    let x = 1;\n    x\n}";
        let (code, how) = extract(reply).expect("extracted");
        assert!(code.contains("let x = 1;"));
        assert_eq!(how, Extraction::WholeMessage);
    }

    #[test]
    fn prose_with_no_code_is_rejected_rather_than_compiled() {
        // Compiling an explanation produces a syntax error pointing at the
        // model's prose, which teaches it nothing. Refusing is more useful.
        for reply in [
            "I think authentication is handled in the auth module. Let me know if you need more detail.",
            "I cannot find anything matching that query.",
            "Here are my thoughts on the design.",
        ] {
            assert!(extract(reply).is_none(), "should have been rejected: {reply}");
        }
    }

    #[test]
    fn an_empty_reply_is_rejected() {
        assert!(extract("").is_none());
        assert!(extract("   \n\n ").is_none());
    }

    #[test]
    fn an_empty_fence_falls_through_to_the_rest_of_the_message() {
        let reply = "```rune\n```\npub fn main() { 8 }";
        let (code, how) = extract(reply).expect("extracted");
        assert_eq!(code, "pub fn main() { 8 }");
        assert_eq!(how, Extraction::WholeMessage);
    }

    #[test]
    fn a_comment_only_reply_is_prose() {
        assert!(extract("// nothing to do here").is_none());
    }
}