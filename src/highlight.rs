//! Syntax highlighting into Shiki-compatible `.line` + Flexoki `.tk-*` spans.
//!
//! Uses `two-face`'s extended TextMate grammar bundle (`nix`, `rust`, `svelte`,
//! `typescript`, `toml`, `bash`, etc.) and maps `syntect`'s `ScopeStack` onto
//! semantic Flexoki token classes so CSS themes switch light/dark cleanly via
//! custom properties without re-rendering HTML.

use std::sync::OnceLock;

use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

/// Escape text for safe inclusion inside HTML elements and attributes.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    push_escaped_html(&mut out, s);
    out
}

pub fn push_escaped_html(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

/// Highlight `code` for `lang`, wrapping each line in `<span class="line">…</span>`
/// separated by `\n` so CSS `.line::before` counters match Shiki line numbering.
pub fn highlight_code(code: &str, lang: &str) -> String {
    let ss = syntax_set();
    let syntax = resolve_syntax(ss, lang);
    let mut parse_state = ParseState::new(syntax);
    let mut scope_stack = ScopeStack::new();

    // Strip a single trailing newline produced by fenced code blocks so an
    // N-line snippet renders exactly N `.line` rows.
    let trimmed = code
        .strip_suffix("\r\n")
        .or_else(|| code.strip_suffix('\n'))
        .unwrap_or(code);
    if trimmed.is_empty() {
        return "<span class=\"line\"></span>".to_string();
    }

    let mut html = String::with_capacity(trimmed.len() * 2 + 64);
    let mut first_line = true;

    for raw_line in LinesWithEndings::from(trimmed) {
        if !first_line {
            html.push('\n');
        }
        first_line = false;
        html.push_str("<span class=\"line\">");

        // Ensure the grammar sees a trailing newline for line-anchored rules.
        let owned_with_nl;
        let parse_input = if raw_line.ends_with('\n') {
            raw_line
        } else {
            owned_with_nl = format!("{raw_line}\n");
            &owned_with_nl
        };

        match parse_state.parse_line(parse_input, ss) {
            Ok(ops) => {
                let mut prev_pos = 0usize;
                let mut pending_class: Option<&'static str> = None;
                let mut pending_text = String::new();

                let flush_run = |out: &mut String, cls: Option<&'static str>, buf: &mut String| {
                    if let Some(c) = cls
                        && !buf.is_empty()
                    {
                        out.push_str("<span class=\"");
                        out.push_str(c);
                        out.push_str("\">");
                        push_escaped_html(out, buf);
                        out.push_str("</span>");
                        buf.clear();
                    }
                };

                for (offset, op) in ops {
                    if offset > prev_pos && prev_pos < raw_line.len() {
                        let end = offset.min(raw_line.len());
                        let slice = strip_line_ending(&raw_line[prev_pos..end]);
                        if !slice.is_empty() {
                            let cls = classify_scopes(&scope_stack);
                            if pending_class == Some(cls) {
                                pending_text.push_str(slice);
                            } else {
                                flush_run(&mut html, pending_class, &mut pending_text);
                                pending_class = Some(cls);
                                pending_text.push_str(slice);
                            }
                        }
                    }
                    prev_pos = offset;
                    let _ = scope_stack.apply(&op);
                }

                if prev_pos < raw_line.len() {
                    let slice = strip_line_ending(&raw_line[prev_pos..]);
                    if !slice.is_empty() {
                        let cls = classify_scopes(&scope_stack);
                        if pending_class == Some(cls) {
                            pending_text.push_str(slice);
                        } else {
                            flush_run(&mut html, pending_class, &mut pending_text);
                            pending_class = Some(cls);
                            pending_text.push_str(slice);
                        }
                    }
                }
                flush_run(&mut html, pending_class, &mut pending_text);
            }
            Err(_) => {
                let slice = strip_line_ending(raw_line);
                if !slice.is_empty() {
                    html.push_str("<span class=\"tk-fg\">");
                    push_escaped_html(&mut html, slice);
                    html.push_str("</span>");
                }
            }
        }

        html.push_str("</span>");
    }

    html
}

fn strip_line_ending(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .or_else(|| s.strip_suffix('\r'))
        .unwrap_or(s)
}

fn resolve_syntax<'a>(ss: &'a SyntaxSet, raw_lang: &str) -> &'a SyntaxReference {
    let token = raw_lang.trim().to_ascii_lowercase();
    if token.is_empty() || token == "text" || token == "txt" || token == "plain" {
        return ss.find_syntax_plain_text();
    }

    let candidates: &[&str] = match token.as_str() {
        "rs" | "rust" => &["rs", "Rust"],
        "nix" => &["nix", "Nix"],
        "sh" | "bash" | "zsh" | "shell" | "console" => &["bash", "sh", "Bourne Again Shell (bash)"],
        "ts" | "typescript" => &["ts", "TypeScript"],
        "tsx" => &["tsx", "TypeScriptReact", "ts"],
        "js" | "javascript" | "mjs" | "cjs" => &["js", "JavaScript"],
        "jsx" => &["jsx", "JavaScript (Babel)", "js"],
        "svelte" => &["svelte", "Svelte", "html"],
        "py" | "python" | "python3" => &["py", "Python"],
        "yml" | "yaml" => &["yaml", "YAML"],
        "toml" => &["toml", "TOML"],
        "json" | "jsonc" | "jsonl" => &["json", "JSON"],
        "html" | "htm" | "xml" | "svg" => &["html", "HTML"],
        "css" => &["css", "CSS"],
        "scss" | "sass" => &["scss", "SCSS", "css"],
        "c" | "h" => &["c", "C"],
        "cpp" | "c++" | "cc" | "cxx" | "hpp" => &["cpp", "C++"],
        "go" | "golang" => &["go", "Go"],
        "sql" | "sqlite" | "postgres" => &["sql", "SQL"],
        "diff" | "patch" => &["diff", "Diff"],
        "md" | "markdown" => &["md", "Markdown"],
        _ => &[],
    };

    for &cand in candidates {
        if let Some(syn) = ss
            .find_syntax_by_token(cand)
            .or_else(|| ss.find_syntax_by_extension(cand))
            .or_else(|| ss.find_syntax_by_name(cand))
        {
            return syn;
        }
    }

    ss.find_syntax_by_token(&token)
        .or_else(|| ss.find_syntax_by_extension(&token))
        .or_else(|| {
            ss.syntaxes()
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(&token))
        })
        .unwrap_or_else(|| ss.find_syntax_plain_text())
}

/// Map the active TextMate `ScopeStack` (from innermost to outermost) to one of
/// the 12 Flexoki token classes.
fn classify_scopes(stack: &ScopeStack) -> &'static str {
    for scope in stack.as_slice().iter().rev() {
        let s = scope.build_string();
        let s = s.as_str();

        if s.starts_with("comment.block.documentation")
            || s.starts_with("comment.line.documentation")
            || s.starts_with("string.quoted.docstring")
            || (s.starts_with("comment") && s.contains("documentation"))
        {
            return "tk-doc";
        }
        if s.starts_with("comment") || s.starts_with("punctuation.definition.comment") {
            return "tk-comment";
        }

        if s.starts_with("meta.annotation")
            || s.starts_with("meta.attribute")
            || s.starts_with("meta.preprocessor")
            || s.starts_with("punctuation.definition.annotation")
            || s.starts_with("entity.name.function.macro")
            || s.starts_with("support.macro")
            || s.starts_with("keyword.control.directive")
            || s.starts_with("keyword.other.at-rule")
            || s.starts_with("meta.diff.header")
            || s.starts_with("meta.diff.range")
        {
            return "tk-meta";
        }

        if s.starts_with("keyword.operator")
            || s.starts_with("storage.type.function.arrow")
            || s.starts_with("punctuation.accessor")
            || s.starts_with("punctuation.separator.key-value")
            || s.starts_with("punctuation.section.interpolation")
            || s.starts_with("punctuation.definition.interpolation")
            || s.starts_with("constant.character.escape")
        {
            return "tk-op";
        }

        if s.starts_with("keyword")
            || s.starts_with("storage.type")
            || s.starts_with("storage.modifier")
            || s.starts_with("markup.deleted")
        {
            return "tk-kw";
        }

        if s.starts_with("constant.numeric")
            || s.starts_with("constant.language")
            || s.starts_with("constant.character")
            || s.starts_with("constant.other.color")
        {
            return "tk-num";
        }

        if s.starts_with("entity.name.function")
            || s.starts_with("support.function")
            || s.starts_with("variable.function")
            || s.starts_with("meta.function-call.generic")
        {
            return "tk-fn";
        }

        if s.starts_with("entity.name.type")
            || s.starts_with("entity.name.struct")
            || s.starts_with("entity.name.enum")
            || s.starts_with("entity.name.trait")
            || s.starts_with("entity.name.interface")
            || s.starts_with("entity.name.class")
            || s.starts_with("entity.name.impl")
            || s.starts_with("support.type.primitive")
            || s.starts_with("support.type.builtin")
        {
            return "tk-type";
        }

        if s.starts_with("support.type.property-name")
            || s.starts_with("punctuation.support.type.property-name")
            || s.starts_with("entity.other.attribute-name")
            || s.starts_with("variable.other.property")
            || s.starts_with("variable.other.member")
            || s.starts_with("variable.other.readwrite.member")
            || s.starts_with("variable.parameter")
            || s.starts_with("meta.object-literal.key")
            || s.starts_with("entity.name.tag.yaml")
        {
            return "tk-prop";
        }

        if s.starts_with("entity.name.tag")
            || s.starts_with("entity.name.section")
            || s.starts_with("entity.name.namespace")
            || s.starts_with("entity.name.module")
            || s.starts_with("entity.other.inherited-class")
            || s.starts_with("support.class")
        {
            return "tk-entity";
        }

        if s.starts_with("support.type") {
            return "tk-type";
        }

        // Stop before an outer `string` scope when inside `${...}` interpolation.
        if s.starts_with("variable")
            || s.starts_with("meta.interpolation")
            || s.starts_with("meta.template.expression")
            || s.starts_with("meta.embedded")
        {
            return "tk-fg";
        }

        if s.starts_with("string")
            || s.starts_with("constant.other.symbol")
            || s.starts_with("markup.inserted")
            || s.starts_with("markup.inline.raw")
        {
            return "tk-str";
        }
    }

    "tk-fg"
}

#[cfg(test)]
mod tests {
    use super::highlight_code;

    #[test]
    fn highlights_rust_and_nix_with_flexoki_classes_and_lines() {
        let rs = "/// Doc line\npub fn greet(name: &str) -> usize {\n    42\n}\n";
        let rs_html = highlight_code(rs, "rust");
        assert_eq!(rs_html.matches("<span class=\"line\">").count(), 4);
        assert!(rs_html.contains("tk-doc") || rs_html.contains("tk-comment"));
        assert!(rs_html.contains("tk-kw"));
        assert!(rs_html.contains("tk-fn"));
        assert!(rs_html.contains("tk-num"));

        let nix =
            "{ pkgs, ... }:\n{\n  services.viewer.enable = true;\n  package = \"hc-viewer\";\n}\n";
        let nix_html = highlight_code(nix, "nix");
        assert_eq!(nix_html.matches("<span class=\"line\">").count(), 5);
        assert!(nix_html.contains("tk-str"));
        assert!(nix_html.contains("tk-num") || nix_html.contains("tk-kw"));
    }

    #[test]
    fn escapes_html_special_chars_inside_tokens() {
        let sh = "if [[ \"$a\" < \"$b\" && -n \"$c\" ]]; then echo '<ok>'; fi";
        let html = highlight_code(sh, "bash");
        assert!(!html.contains("<ok>"));
        assert!(html.contains("&lt;ok&gt;"));
        assert!(html.contains("&amp;&amp;"));
    }
}
