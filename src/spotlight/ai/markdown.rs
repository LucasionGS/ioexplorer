//! A small Markdown subset for rendering completed assistant replies.
//!
//! Pure over `&str` — block splitting and inline-to-Pango-markup conversion are
//! both ordinary functions, so the whole thing is unit-testable without GTK.
//!
//! Scope is deliberately narrow: what models actually emit in a chat reply.
//! Anything unrecognised falls through as literal text rather than being
//! swallowed, so a reply can never lose content to a parsing gap.

/// One block-level element of a reply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Block {
    /// `#` through `######`. Levels below 3 are rendered like a level-3.
    Heading {
        level: u8,
        text: String,
    },
    Paragraph(String),
    /// A fenced code block. An unterminated fence still yields one, so a reply
    /// cut off mid-snippet keeps its code.
    Code {
        lang: Option<String>,
        body: String,
    },
    /// `-`, `*` or `+` bullet.
    Bullet {
        indent: usize,
        text: String,
    },
    /// `1.` style item; the marker is kept verbatim so numbering survives.
    Numbered {
        indent: usize,
        marker: String,
        text: String,
    },
    Quote(String),
    Rule,
}

/// Splits a completed message into block-level elements.
pub fn parse(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        if let Some(lang) = fence_language(trimmed) {
            flush_paragraph(&mut blocks, &mut paragraph);
            let mut body = Vec::new();
            // An unterminated fence consumes the rest — the model was cut off.
            for line in lines.by_ref() {
                if fence_language(line.trim_start()).is_some() {
                    break;
                }
                body.push(line);
            }
            blocks.push(Block::Code {
                lang,
                body: body.join("\n"),
            });
            continue;
        }

        if trimmed.is_empty() {
            flush_paragraph(&mut blocks, &mut paragraph);
            continue;
        }

        if is_rule(trimmed) {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(Block::Rule);
            continue;
        }

        if let Some((level, rest)) = heading(trimmed) {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(Block::Heading {
                level,
                text: rest.to_string(),
            });
            continue;
        }

        if let Some(rest) = bullet(trimmed) {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(Block::Bullet {
                indent,
                text: rest.to_string(),
            });
            continue;
        }

        if let Some((marker, rest)) = numbered(trimmed) {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(Block::Numbered {
                indent,
                marker: marker.to_string(),
                text: rest.to_string(),
            });
            continue;
        }

        if let Some(rest) = trimmed
            .strip_prefix("> ")
            .or_else(|| trimmed.strip_prefix(">"))
        {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(Block::Quote(rest.trim_start().to_string()));
            continue;
        }

        paragraph.push(trimmed);
    }

    flush_paragraph(&mut blocks, &mut paragraph);
    blocks
}

fn flush_paragraph(blocks: &mut Vec<Block>, paragraph: &mut Vec<&str>) {
    if !paragraph.is_empty() {
        blocks.push(Block::Paragraph(paragraph.join(" ")));
        paragraph.clear();
    }
}

/// `Some(lang)` when the line opens or closes a fence; `lang` is `None` for a
/// bare ``` and for closing fences.
fn fence_language(line: &str) -> Option<Option<String>> {
    let rest = line.strip_prefix("```")?;
    let rest = rest.trim();
    Some((!rest.is_empty()).then(|| rest.to_string()))
}

fn is_rule(line: &str) -> bool {
    let line = line.trim_end();
    line.len() >= 3
        && (line.chars().all(|c| c == '-')
            || line.chars().all(|c| c == '*')
            || line.chars().all(|c| c == '_'))
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?;
    Some((hashes as u8, rest.trim()))
}

fn bullet(line: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest.trim_start());
        }
    }
    None
}

fn numbered(line: &str) -> Option<(&str, &str)> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = line[digits..].strip_prefix(". ")?;
    Some((&line[..digits], rest.trim_start()))
}

/// Converts inline Markdown to Pango markup, escaping everything else.
///
/// Supported: `` `code` ``, `**bold**`, `*italic*`, `~~strike~~` and
/// `[text](url)`. An unclosed marker renders literally so no text is lost.
///
/// `_` is deliberately **not** emphasis: `snake_case_names` are far more common
/// than underscore italics in the replies this renders, and mangling them would
/// be worse than missing a rare italic.
pub fn inline_markup(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;

    while index < chars.len() {
        // Code spans win: no other marker applies inside one.
        if chars[index] == '`'
            && let Some(end) = find(&chars, index + 1, &['`'])
        {
            out.push_str("<tt>");
            escape_into(&mut out, &chars[index + 1..end]);
            out.push_str("</tt>");
            index = end + 1;
            continue;
        }

        if starts_with(&chars, index, "**")
            && let Some(end) = find_seq(&chars, index + 2, "**")
        {
            out.push_str("<b>");
            out.push_str(&inline_markup(&collect(&chars[index + 2..end])));
            out.push_str("</b>");
            index = end + 2;
            continue;
        }

        if starts_with(&chars, index, "~~")
            && let Some(end) = find_seq(&chars, index + 2, "~~")
        {
            out.push_str("<s>");
            out.push_str(&inline_markup(&collect(&chars[index + 2..end])));
            out.push_str("</s>");
            index = end + 2;
            continue;
        }

        if chars[index] == '*'
            && !starts_with(&chars, index, "**")
            && let Some(end) = find(&chars, index + 1, &['*'])
            && end > index + 1
        {
            out.push_str("<i>");
            out.push_str(&inline_markup(&collect(&chars[index + 1..end])));
            out.push_str("</i>");
            index = end + 1;
            continue;
        }

        // [text](url) — the label is kept, the target underlined rather than
        // made clickable, since a chat bubble has nowhere to navigate to.
        if chars[index] == '['
            && let Some(close) = find(&chars, index + 1, &[']'])
            && starts_with(&chars, close + 1, "(")
            && let Some(paren) = find(&chars, close + 2, &[')'])
        {
            out.push_str("<u>");
            out.push_str(&inline_markup(&collect(&chars[index + 1..close])));
            out.push_str("</u>");
            index = paren + 1;
            continue;
        }

        escape_into(&mut out, &chars[index..index + 1]);
        index += 1;
    }

    out
}

fn collect(chars: &[char]) -> String {
    chars.iter().collect()
}

fn starts_with(chars: &[char], index: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(offset, expected)| chars.get(index + offset) == Some(&expected))
}

fn find(chars: &[char], from: usize, any_of: &[char]) -> Option<usize> {
    (from..chars.len()).find(|index| any_of.contains(&chars[*index]))
}

fn find_seq(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    (from..chars.len()).find(|index| starts_with(chars, *index, needle))
}

/// Pango markup is XML, so these five characters must never pass through raw.
fn escape_into(out: &mut String, chars: &[char]) {
    for ch in chars {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            other => out.push(*other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paragraph(text: &str) -> Block {
        Block::Paragraph(text.to_string())
    }

    #[test]
    fn plain_text_is_one_paragraph() {
        assert_eq!(parse("just some text"), vec![paragraph("just some text")]);
    }

    #[test]
    fn blank_lines_separate_paragraphs() {
        assert_eq!(
            parse("first\n\nsecond"),
            vec![paragraph("first"), paragraph("second")]
        );
    }

    #[test]
    fn wrapped_lines_join_into_one_paragraph() {
        assert_eq!(parse("one\ntwo"), vec![paragraph("one two")]);
    }

    #[test]
    fn parses_headings_at_every_level() {
        assert_eq!(
            parse("# Title"),
            vec![Block::Heading {
                level: 1,
                text: "Title".to_string()
            }]
        );
        assert_eq!(
            parse("### Deep"),
            vec![Block::Heading {
                level: 3,
                text: "Deep".to_string()
            }]
        );
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        assert_eq!(parse("#hashtag"), vec![paragraph("#hashtag")]);
        assert_eq!(
            parse("####### too many"),
            vec![paragraph("####### too many")]
        );
    }

    #[test]
    fn parses_fenced_code_with_a_language() {
        assert_eq!(
            parse("```rust\nfn main() {}\n```"),
            vec![Block::Code {
                lang: Some("rust".to_string()),
                body: "fn main() {}".to_string()
            }]
        );
    }

    #[test]
    fn an_unterminated_fence_still_yields_its_code() {
        assert_eq!(
            parse("```\ncut off"),
            vec![Block::Code {
                lang: None,
                body: "cut off".to_string()
            }]
        );
    }

    #[test]
    fn markdown_inside_a_fence_is_not_parsed() {
        assert_eq!(
            parse("```\n# not a heading\n- not a bullet\n```"),
            vec![Block::Code {
                lang: None,
                body: "# not a heading\n- not a bullet".to_string()
            }]
        );
    }

    #[test]
    fn parses_bullets_and_numbered_items() {
        assert_eq!(
            parse("- one\n- two"),
            vec![
                Block::Bullet {
                    indent: 0,
                    text: "one".to_string()
                },
                Block::Bullet {
                    indent: 0,
                    text: "two".to_string()
                },
            ]
        );
        assert_eq!(
            parse("1. first"),
            vec![Block::Numbered {
                indent: 0,
                marker: "1".to_string(),
                text: "first".to_string()
            }]
        );
    }

    #[test]
    fn records_list_indentation() {
        assert_eq!(
            parse("  - nested"),
            vec![Block::Bullet {
                indent: 2,
                text: "nested".to_string()
            }]
        );
    }

    #[test]
    fn parses_quotes_and_rules() {
        assert_eq!(parse("> quoted"), vec![Block::Quote("quoted".to_string())]);
        assert_eq!(parse("---"), vec![Block::Rule]);
    }

    #[test]
    fn a_dash_bullet_is_not_mistaken_for_a_rule() {
        assert_eq!(
            parse("- item"),
            vec![Block::Bullet {
                indent: 0,
                text: "item".to_string()
            }]
        );
    }

    #[test]
    fn converts_bold_italic_and_code_spans() {
        assert_eq!(inline_markup("**bold**"), "<b>bold</b>");
        assert_eq!(inline_markup("*italic*"), "<i>italic</i>");
        assert_eq!(inline_markup("`code`"), "<tt>code</tt>");
        assert_eq!(inline_markup("~~gone~~"), "<s>gone</s>");
    }

    #[test]
    fn nests_inline_markers() {
        assert_eq!(
            inline_markup("**bold with *italic* inside**"),
            "<b>bold with <i>italic</i> inside</b>"
        );
    }

    #[test]
    fn a_code_span_suppresses_other_markers() {
        assert_eq!(inline_markup("`**not bold**`"), "<tt>**not bold**</tt>");
    }

    #[test]
    fn underscores_are_literal_so_snake_case_survives() {
        assert_eq!(
            inline_markup("call some_function_name now"),
            "call some_function_name now"
        );
        assert_eq!(inline_markup("__not bold__"), "__not bold__");
    }

    #[test]
    fn an_unclosed_marker_renders_literally() {
        assert_eq!(inline_markup("**not closed"), "**not closed");
        assert_eq!(inline_markup("a * b"), "a * b");
        assert_eq!(inline_markup("`unclosed"), "`unclosed");
    }

    #[test]
    fn escapes_xml_so_markup_cannot_be_injected() {
        assert_eq!(inline_markup("a < b & c > d"), "a &lt; b &amp; c &gt; d");
        // A model emitting Pango-looking text must not become real markup.
        assert_eq!(
            inline_markup("<span foreground=\"red\">x</span>"),
            "&lt;span foreground=&quot;red&quot;&gt;x&lt;/span&gt;"
        );
    }

    #[test]
    fn escapes_inside_emphasis_too() {
        assert_eq!(inline_markup("**a < b**"), "<b>a &lt; b</b>");
        assert_eq!(inline_markup("`a & b`"), "<tt>a &amp; b</tt>");
    }

    #[test]
    fn keeps_link_text_and_drops_the_target() {
        assert_eq!(
            inline_markup("see [the docs](https://example.com) now"),
            "see <u>the docs</u> now"
        );
    }

    #[test]
    fn a_malformed_link_renders_literally() {
        assert_eq!(inline_markup("[no target]"), "[no target]");
    }

    #[test]
    fn handles_a_realistic_reply() {
        let blocks = parse(
            "# Rotating a PDF\n\nUse **qpdf**:\n\n```sh\nqpdf in.pdf out.pdf --rotate=90\n```\n\n- Works offline\n- Keeps text selectable\n",
        );

        assert_eq!(blocks.len(), 5);
        assert!(matches!(blocks[0], Block::Heading { level: 1, .. }));
        assert!(matches!(blocks[1], Block::Paragraph(_)));
        assert!(matches!(blocks[2], Block::Code { .. }));
        assert!(matches!(blocks[3], Block::Bullet { .. }));
        assert!(matches!(blocks[4], Block::Bullet { .. }));
    }

    #[test]
    fn empty_input_yields_no_blocks() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n").is_empty());
    }
}
