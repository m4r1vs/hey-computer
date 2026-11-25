//! Markdown-to-HTML rendering pipeline with native MathML, GFM callouts, and
//! interactive Shiki/Flexoki code blocks.

use pulldown_cmark::{BlockQuoteKind, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use pulldown_latex::config::{DisplayMode, RenderConfig};

use crate::highlight::{escape_html, highlight_code};
use crate::present::disarm_html;

/// Whether a fenced code block language supports click-to-run in the viewer.
pub fn is_runnable_lang(lang: &str) -> bool {
    matches!(lang, "sh" | "bash" | "zsh" | "shell" | "python" | "py")
}

/// Convert raw Markdown into `<div class="markdown-content">…</div>` after
/// disarming untrusted HTML and remote images.
pub fn render_markdown(markdown: &str) -> String {
    let safe_md = disarm_html(markdown);
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_GFM;

    let raw_events: Vec<Event<'_>> = Parser::new_ext(&safe_md, opts).collect();
    let mut transformed: Vec<Event<'_>> = Vec::with_capacity(raw_events.len());
    let mut blockquote_stack: Vec<Option<CalloutKind>> = Vec::new();
    let mut code_index = 0usize;

    let mut i = 0usize;
    while i < raw_events.len() {
        match &raw_events[i] {
            Event::Start(Tag::BlockQuote(gfm_kind)) => {
                if let Some(callout) = gfm_kind.and_then(CalloutKind::from_gfm) {
                    blockquote_stack.push(Some(callout));
                    transformed.push(Event::Html(callout.open_html().into()));
                    i += 1;
                    continue;
                }

                // Fallback for `[!NOTE]` written on the same line as body text.
                if let Some((callout, strip_len)) = detect_manual_callout(&raw_events[i + 1..]) {
                    blockquote_stack.push(Some(callout));
                    transformed.push(Event::Html(callout.open_html().into()));
                    i += 1;
                    strip_callout_marker_from_first_paragraph(
                        &raw_events,
                        &mut i,
                        strip_len,
                        &mut transformed,
                    );
                    continue;
                }

                blockquote_stack.push(None);
                transformed.push(Event::Start(Tag::BlockQuote(None)));
                i += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                match blockquote_stack.pop().flatten() {
                    Some(_) => transformed.push(Event::Html("</div></aside>\n".into())),
                    None => transformed.push(Event::End(TagEnd::BlockQuote(None))),
                }
                i += 1;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang_raw = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").trim()
                    }
                    CodeBlockKind::Indented => "",
                };
                let lang = if lang_raw.is_empty() {
                    "text".to_string()
                } else {
                    lang_raw.to_ascii_lowercase()
                };
                i += 1;
                let mut code_buf = String::new();
                while i < raw_events.len() {
                    match &raw_events[i] {
                        Event::End(TagEnd::CodeBlock) => {
                            i += 1;
                            break;
                        }
                        Event::Text(t) | Event::Code(t) => {
                            code_buf.push_str(t);
                            i += 1;
                        }
                        Event::SoftBreak | Event::HardBreak => {
                            code_buf.push('\n');
                            i += 1;
                        }
                        _ => i += 1,
                    }
                }
                let block_html = render_code_block(code_index, &lang, &code_buf);
                code_index += 1;
                transformed.push(Event::Html(block_html.into()));
            }
            Event::InlineMath(tex) => {
                transformed.push(Event::Html(render_math(tex, false).into()));
                i += 1;
            }
            Event::DisplayMath(tex) => {
                transformed.push(Event::Html(render_math(tex, true).into()));
                i += 1;
            }
            // Defense in depth: any raw HTML event from Markdown is escaped as text.
            Event::Html(raw) | Event::InlineHtml(raw) => {
                transformed.push(Event::Text(raw.clone()));
                i += 1;
            }
            other => {
                transformed.push(other.clone());
                i += 1;
            }
        }
    }

    let mut inner = String::with_capacity(safe_md.len() * 2 + 64);
    pulldown_cmark::html::push_html(&mut inner, transformed.into_iter());

    let mut out = String::with_capacity(inner.len() + 40);
    out.push_str("<div class=\"markdown-content\">");
    out.push_str(&inner);
    out.push_str("</div>");
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalloutKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl CalloutKind {
    fn from_gfm(kind: BlockQuoteKind) -> Option<Self> {
        match kind {
            BlockQuoteKind::Note => Some(Self::Note),
            BlockQuoteKind::Tip => Some(Self::Tip),
            BlockQuoteKind::Important => Some(Self::Important),
            BlockQuoteKind::Warning => Some(Self::Warning),
            BlockQuoteKind::Caution => Some(Self::Caution),
        }
    }

    fn parse_tag(s: &str) -> Option<(Self, usize)> {
        let candidates = [
            ("[!NOTE]", Self::Note),
            ("[!TIP]", Self::Tip),
            ("[!IMPORTANT]", Self::Important),
            ("[!WARNING]", Self::Warning),
            ("[!CAUTION]", Self::Caution),
        ];
        let trimmed = s.trim_start();
        let lead = s.len() - trimmed.len();
        for (tag, kind) in candidates {
            if trimmed
                .get(..tag.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(tag))
            {
                let after = &trimmed[tag.len()..];
                if after.is_empty() || after.starts_with(char::is_whitespace) {
                    let ws = after
                        .bytes()
                        .take_while(|&b| b == b' ' || b == b'\t')
                        .count();
                    return Some((kind, lead + tag.len() + ws));
                }
            }
        }
        None
    }

    fn slug_and_title(self) -> (&'static str, &'static str) {
        match self {
            Self::Note => ("callout-note", "Note"),
            Self::Tip => ("callout-tip", "Tip"),
            Self::Important => ("callout-important", "Important"),
            Self::Warning => ("callout-warning", "Warning"),
            Self::Caution => ("callout-caution", "Caution"),
        }
    }

    fn open_html(self) -> String {
        let (slug, title) = self.slug_and_title();
        format!(
            "<aside class=\"callout {slug}\"><div class=\"callout-title\">{title}</div><div class=\"callout-body\">"
        )
    }
}

fn detect_manual_callout(events: &[Event<'_>]) -> Option<(CalloutKind, usize)> {
    match (events.first(), events.get(1)) {
        (Some(Event::Start(Tag::Paragraph)), Some(Event::Text(text))) => {
            CalloutKind::parse_tag(text)
        }
        _ => None,
    }
}

fn strip_callout_marker_from_first_paragraph<'a>(
    events: &[Event<'a>],
    idx: &mut usize,
    strip_len: usize,
    out: &mut Vec<Event<'a>>,
) {
    if let (Some(Event::Start(Tag::Paragraph)), Some(Event::Text(first_text))) =
        (events.get(*idx), events.get(*idx + 1))
    {
        let remainder = &first_text[strip_len.min(first_text.len())..];
        out.push(Event::Start(Tag::Paragraph));
        *idx += 2;
        if !remainder.is_empty() {
            out.push(Event::Text(remainder.to_string().into()));
        } else if matches!(events.get(*idx), Some(Event::SoftBreak | Event::HardBreak)) {
            *idx += 1;
        }
    }
}

/// `pulldown-latex` 0.8 writes `<` and `>` raw inside `<mo><</mo>` for
/// `Content::Relation`. Escape any non-tag `<` / `>` in text positions so the
/// resulting MathML is valid, safe HTML5.
fn escape_mathml_text_angles(mathml: &str) -> String {
    let mut out = String::with_capacity(mathml.len() + 8);
    let bytes = mathml.as_bytes();
    let mut in_tag = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_tag {
            if b == b'>' {
                in_tag = false;
            }
            out.push(b as char);
            i += 1;
        } else if b == b'<' {
            let next = bytes.get(i + 1).copied().unwrap_or(0);
            if next == b'/' || next.is_ascii_alphabetic() {
                in_tag = true;
                out.push('<');
            } else {
                out.push_str("&lt;");
            }
            i += 1;
        } else if b == b'>' {
            out.push_str("&gt;");
            i += 1;
        } else {
            let ch = mathml[i..].chars().next().unwrap_or('\0');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn render_math(latex: &str, display: bool) -> String {
    // `present::disarm_html` rewrites `<` inside math spans to `\lt `; restore
    // `<` for `pulldown-latex` when followed by non-letters, or let `\lt` work
    // natively (`pulldown-latex` supports both `\lt` and `<`).
    let trimmed = latex.trim();
    if trimmed.is_empty() {
        return if display {
            "<div class=\"math-display\"><code></code></div>".to_string()
        } else {
            "<span class=\"math-inline\"><code></code></span>".to_string()
        };
    }

    let storage = pulldown_latex::Storage::new();
    let check_parser = pulldown_latex::Parser::new(trimmed, &storage);
    let has_error = check_parser.into_iter().any(|ev| ev.is_err());

    if !has_error {
        let parser = pulldown_latex::Parser::new(trimmed, &storage);
        let config = RenderConfig {
            display_mode: if display {
                DisplayMode::Block
            } else {
                DisplayMode::Inline
            },
            ..RenderConfig::default()
        };
        let mut raw_mathml = String::new();
        if pulldown_latex::push_mathml(&mut raw_mathml, parser, config).is_ok()
            && raw_mathml.contains("<math")
            && !raw_mathml.contains("<merror")
        {
            let mathml = escape_mathml_text_angles(&raw_mathml);
            return if display {
                format!("<div class=\"math-display\">{mathml}</div>")
            } else {
                format!("<span class=\"math-inline\">{mathml}</span>")
            };
        }
    }

    let escaped = escape_html(trimmed);
    if display {
        format!("<div class=\"math-display\"><code>{escaped}</code></div>")
    } else {
        format!("<span class=\"math-inline\"><code>{escaped}</code></span>")
    }
}

fn render_code_block(index: usize, lang: &str, raw_code: &str) -> String {
    let runnable = is_runnable_lang(lang);
    let clean_raw = raw_code
        .strip_suffix("\r\n")
        .or_else(|| raw_code.strip_suffix('\n'))
        .unwrap_or(raw_code);
    let escaped_raw = escape_html(clean_raw);
    let escaped_lang = escape_html(lang);
    let highlighted = highlight_code(raw_code, lang);
    let run_btn = if runnable {
        "\n    <button type=\"button\" class=\"code-btn code-btn-run\" data-action=\"run-code\" title=\"Run snippet\">▶ Run</button>"
    } else {
        ""
    };

    format!(
        "<div class=\"code-block\" data-code-index=\"{index}\" data-lang=\"{escaped_lang}\" data-runnable=\"{runnable}\" data-raw=\"{escaped_raw}\">\n  <div class=\"code-toolbar\">\n    <span class=\"code-lang\">{escaped_lang}</span>\n    <button type=\"button\" class=\"code-btn\" data-action=\"copy-code\" title=\"Copy code\">Copy</button>{run_btn}\n  </div>\n  <pre class=\"shiki-flexoki\"><code>{highlighted}</code></pre>\n  <div class=\"code-output\" hidden></div>\n</div>\n"
    )
}

#[cfg(test)]
mod tests {
    use super::render_markdown;

    #[test]
    fn renders_inline_and_display_mathml_and_falls_back_on_error() {
        let md = "Inline $E = mc^2$ and bound $a < b$.\n\n$$\\int_{-\\infty}^{\\infty} e^{-x^2}\\,dx = \\sqrt{\\pi}$$\n\nBroken: $\\unknownmacro{x}$\n";
        let html = render_markdown(md);
        assert!(
            html.starts_with("<div class=\"markdown-content\">"),
            "{html}"
        );
        assert!(html.contains("<span class=\"math-inline\"><math"), "{html}");
        assert!(
            html.contains("<div class=\"math-display\"><math display=\"block\""),
            "{html}"
        );
        assert!(html.contains("&lt;"), "{html}");
        assert!(
            html.contains("<span class=\"math-inline\"><code>\\unknownmacro{x}</code></span>"),
            "{html}"
        );
    }

    #[test]
    fn renders_nix_rust_and_runnable_bash_code_blocks() {
        let md = "```nix\n{ pkgs }: pkgs.hello\n```\n\n```rust\nfn main() {}\n```\n\n```bash\nuname -srm\n```\n";
        let html = render_markdown(md);
        assert!(html.contains("data-code-index=\"0\" data-lang=\"nix\" data-runnable=\"false\""));
        assert!(html.contains("data-code-index=\"1\" data-lang=\"rust\" data-runnable=\"false\""));
        assert!(html.contains("data-code-index=\"2\" data-lang=\"bash\" data-runnable=\"true\""));
        assert!(html.contains("data-action=\"run-code\""));
        assert!(html.contains("<pre class=\"shiki-flexoki\"><code><span class=\"line\">"));
    }

    #[test]
    fn renders_gfm_callouts_tables_and_disarmed_remote_images() {
        let md = "> [!TIP]\n> Prefer warm spare sessions.\n\n> [!WARNING]\n> Token cap approaching.\n\n| Mode | Cold | Warm |\n|---|---:|---:|\n| ACP | 940 ms | 32 ms |\n\n![Graph](https://example.org/plot.png)\n<script>evil()</script>\n";
        let html = render_markdown(md);
        assert!(html.contains("<aside class=\"callout callout-tip\"><div class=\"callout-title\">Tip</div><div class=\"callout-body\">"));
        assert!(html.contains("<aside class=\"callout callout-warning\"><div class=\"callout-title\">Warning</div><div class=\"callout-body\">"));
        assert!(html.contains("<table>"));
        assert!(html.contains("940 ms"));
        assert!(!html.contains("<img"));
        assert!(html.contains("href=\"https://example.org/plot.png\""));
        assert!(html.contains("image: Graph"));
        assert!(!html.contains("<script>"));
    }
}
