//! `anubis fmt`: formats ANUBIS files from the token stream, not an AST.
//!
//! The lexer skips whitespace and comments, so they are exactly the text
//! between token spans. The formatter keeps that text's comments and the
//! author's line breaks, and normalizes the rest:
//!
//! - indentation: one level per line that opened a bracket still open;
//! - spacing within a line (`a = b`, `f(x)`, `[ a, b ]`, `{ a = b }`);
//! - a trailing comma in every multi-line list, none in a one-line list
//!   (a `select(...)` is not a list);
//! - at most one blank line in a row, none just inside a bracket;
//! - comments after code on a line keep their spacing (so aligned columns stay aligned);
//! - lists of plain strings in [`SORTED_FIELDS`] are sorted, including the
//!   literal part of `srcs = [...] + select(...)`. A multi-line
//!   list sorts within runs separated by blank lines, and each item takes its
//!   comments with it.
//!
//! [`format`] checks its own output: it must parse to the same value as the
//! input (sorted fields compared sorted), or the file is left alone.

use crate::papyrus::{read_papyrus_str, Token, Value};
use crate::{anyhow_loc, bail_loc, function_name};
use logos::Logos;

/// Fields whose list order doesn't matter (buildifier sorts the same two).
/// Not `libraries` or `*_defines`: link order picks between duplicate
/// symbols, and a repeated define's last value wins.
pub const SORTED_FIELDS: &[&str] = &["srcs", "deps"];

const INDENT: &str = "    ";

/// A line of trivia before a token: a blank line or a whole-line comment.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Line<'a> {
    Blank,
    Comment(&'a str),
}

#[derive(Clone, Debug)]
struct Tok<'a> {
    tok: Token<'a>,
    text: &'a str,
    /// Starts a new line in the source.
    newline_before: bool,
    /// Blank lines and whole-line comments between the previous token's line and this one.
    leading: Vec<Line<'a>>,
    /// A comment after this token on the same line, with the whitespace before it.
    trailing: Option<&'a str>,
}

fn is_opener(t: &Token) -> bool {
    matches!(t, Token::ParenOpen | Token::BracketOpen | Token::BraceOpen)
}

fn is_closer(t: &Token) -> bool {
    matches!(t, Token::ParenClose | Token::BracketClose | Token::BraceClose)
}

/// Formats one file. Errors if it doesn't lex or parse, or if the result
/// wouldn't mean the same thing (a formatter bug; the file is left as is).
pub fn format(src: &str) -> anyhow::Result<String> {
    let mut toks = tokenize(src)?;
    let tail = trailing_lines(src, &mut toks);
    let pairs = match_brackets(&toks)?;
    let mut toks = normalize_commas(toks, &pairs);
    let pairs = match_brackets(&toks)?;
    sort_lists(&mut toks, &pairs);
    let mut out = emit(&toks, &tail);
    // Keep Windows line endings (e.g. a checkout with core.autocrlf).
    if src.contains("\r\n") {
        out = out.replace('\n', "\r\n");
    }

    let before = read_papyrus_str(src, "input")?;
    let after = read_papyrus_str(&out, "formatted")
        .map_err(|e| anyhow_loc!("formatter produced invalid Papyrus: {:#}", e))?;
    if normalized(before) != normalized(after) {
        bail_loc!("formatting would change the file's meaning; left as is");
    }
    Ok(out)
}

fn tokenize(src: &str) -> anyhow::Result<Vec<Tok<'_>>> {
    let mut toks: Vec<Tok> = Vec::new();
    let mut prev_end = 0;
    for (tok, span) in Token::lexer(src).spanned() {
        let tok =
            tok.map_err(|_| anyhow_loc!("can't lex [{}] at byte {}", &src[span.clone()], span.start))?;
        let gap = &src[prev_end..span.start];
        let (trailing, newline_before, leading) = split_gap(gap, toks.is_empty(), false);
        if let (Some(t), Some(prev)) = (trailing, toks.last_mut()) {
            prev.trailing = Some(t);
        }
        toks.push(Tok {
            tok,
            text: &src[span.clone()],
            newline_before: newline_before || toks.is_empty(),
            leading,
            trailing: None,
        });
        prev_end = span.end;
    }
    Ok(toks)
}

/// Splits the text between two tokens into the previous token's trailing
/// comment, whether a newline separates them, and the lines in between.
fn split_gap(gap: &str, at_start: bool, at_end: bool) -> (Option<&str>, bool, Vec<Line<'_>>) {
    let mut segments: Vec<&str> = gap.split('\n').collect();
    if !at_end {
        segments.pop(); // indentation before the next token
    }
    let newline = gap.contains('\n') || at_end;
    let mut trailing = None;
    if !at_start && newline {
        let first = segments.remove(0).trim_end_matches('\r');
        if first.contains('#') {
            trailing = Some(first.trim_end());
        }
    }
    let lines = segments
        .iter()
        .map(|s| match s.trim() {
            "" => Line::Blank,
            c => Line::Comment(c),
        })
        .collect();
    (trailing, newline, lines)
}

/// The comment after the last token and the lines after it.
fn trailing_lines<'a>(src: &'a str, toks: &mut [Tok<'a>]) -> Vec<Line<'a>> {
    let end = toks.last().map_or(0, |t| {
        t.text.as_ptr() as usize - src.as_ptr() as usize + t.text.len()
    });
    let (trailing, _, lines) = split_gap(&src[end..], toks.is_empty(), true);
    if let (Some(t), Some(last)) = (trailing, toks.last_mut()) {
        last.trailing = Some(t);
    }
    lines
}

/// `pairs[closer] = opener` (and `usize::MAX` for other tokens).
fn match_brackets(toks: &[Tok]) -> anyhow::Result<Vec<usize>> {
    let mut pairs = vec![usize::MAX; toks.len()];
    let mut stack = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if is_opener(&t.tok) {
            stack.push(i);
        } else if is_closer(&t.tok) {
            pairs[i] = stack.pop().ok_or_else(|| anyhow_loc!("unmatched [{}]", t.text))?;
        }
    }
    if let Some(&i) = stack.last() {
        bail_loc!("unclosed [{}]", toks[i].text);
    }
    Ok(pairs)
}

/// Adds a trailing comma to each multi-line list and drops it from one-line lists.
fn normalize_commas<'a>(toks: Vec<Tok<'a>>, pairs: &[usize]) -> Vec<Tok<'a>> {
    let mut add = vec![false; toks.len()];
    let mut drop = vec![false; toks.len()];
    for c in 0..toks.len() {
        // Skip non-closers, empty groups, and `select(...)`, which holds one
        // `(inputs) => { ... }` rather than a list.
        if !is_closer(&toks[c].tok)
            || pairs[c] == c - 1
            || (pairs[c] > 0 && matches!(toks[pairs[c] - 1].tok, Token::Select | Token::MultiSelect))
        {
            continue;
        }
        let is_comma = toks[c - 1].tok == Token::Comma;
        if toks[c].newline_before && !is_comma {
            add[c - 1] = true;
        } else if !toks[c].newline_before && is_comma && toks[c - 1].trailing.is_none() {
            drop[c - 1] = true;
        }
    }
    let mut out = Vec::with_capacity(toks.len());
    for (i, mut t) in toks.into_iter().enumerate() {
        if drop[i] {
            continue;
        }
        let trailing = if add[i] { t.trailing.take() } else { None };
        out.push(t);
        if add[i] {
            out.push(Tok {
                tok: Token::Comma,
                text: ",",
                newline_before: false,
                leading: vec![],
                trailing,
            });
        }
    }
    out
}

/// Sorts `field = [...]`, `field = Targets([...])` and `field = RelPaths([...])`
/// for fields in [`SORTED_FIELDS`], when every item is `"x"`, `RelPath("x")`
/// or `Target("x")` on a line of its own (or all on one line).
fn sort_lists(toks: &mut [Tok], pairs: &[usize]) {
    use Token::*;
    for c in 0..toks.len() {
        let o = pairs[c];
        if toks[c].tok != BracketClose || c == o + 1 {
            continue;
        }
        let tok = |i: Option<usize>| i.map(|i| &toks[i].tok);
        let field = match (tok(o.checked_sub(1)), tok(o.checked_sub(2))) {
            (Some(Equals), _) => o.checked_sub(2),
            (Some(ParenOpen), Some(Targets | RelPaths)) if tok(o.checked_sub(3)) == Some(&Equals) => o.checked_sub(4),
            _ => None,
        };
        if !matches!(tok(field), Some(Identifier(name)) if SORTED_FIELDS.contains(name)) {
            continue;
        }

        // Units: an item and the comma after it, if any.
        let mut units: Vec<Vec<Tok>> = Vec::new();
        for t in &toks[o + 1..c] {
            match units.last_mut() {
                Some(u) if u.last().unwrap().tok != Comma => u.push(t.clone()),
                _ => units.push(vec![t.clone()]),
            }
        }
        let multiline = toks[c].newline_before;
        let simple = units.iter().all(|u| {
            u.iter().enumerate().all(|(i, t)| {
                matches!(t.tok, String(_) | RelPath | Target | ParenOpen | ParenClose | Comma)
                    && t.newline_before == (multiline && i == 0)
            })
        });
        if !simple {
            continue;
        }
        let key = |u: &Vec<Tok>| u.iter().filter(|t| t.tok != Comma).map(|t| t.text).collect::<std::string::String>();
        let has_comma = |u: &Vec<Tok>| u.last().unwrap().tok == Comma;
        let comma = units.iter().find(|u| has_comma(u)).map(|u| u.last().unwrap().clone());
        let trailing_comma = has_comma(units.last().unwrap());

        // Sort each run of items between blank lines on its own (a one-line
        // list is one run). A run's blank line stays on top; comments above
        // an item move with it.
        let mut start = 0;
        for i in 1..=units.len() {
            if i == units.len() || units[i][0].leading.contains(&Line::Blank) {
                let leading = &mut units[start][0].leading;
                let blank = leading.first() == Some(&Line::Blank);
                while leading.first() == Some(&Line::Blank) {
                    leading.remove(0);
                }
                units[start..i].sort_by_key(key);
                if blank {
                    units[start][0].leading.insert(0, Line::Blank);
                }
                start = i;
            }
        }
        // Every item but the last has a comma; the last keeps the list's.
        let n = units.len();
        for (i, u) in units.iter_mut().enumerate() {
            match (has_comma(u), i + 1 < n || trailing_comma) {
                (true, false) => drop(u.pop()),
                (false, true) => u.push(comma.clone().expect("a list of several items has a comma")),
                _ => {}
            }
        }
        for (i, t) in units.into_iter().flatten().enumerate() {
            toks[o + 1 + i] = t;
        }
    }
}

fn space_between(prev: &Token, cur: &Token, empty_group: bool) -> &'static str {
    use Token::*;
    if empty_group {
        return "";
    }
    match (prev, cur) {
        (_, Comma | ParenClose) | (ParenOpen, _) => "",
        (Identifier(_) | RelPath | RelPaths | Target | Targets | Glob | Select | MultiSelect, ParenOpen) => {
            ""
        }
        _ => " ",
    }
}

fn emit(toks: &[Tok], tail: &[Line]) -> String {
    let mut out = String::new();
    // Indent of the line each open bracket was opened on.
    let mut open: Vec<usize> = Vec::new();
    let mut line_indent = 0;
    let mut prev: Option<&Tok> = None;
    for t in toks {
        let closer = is_closer(&t.tok);
        if t.newline_before {
            let inner = open.last().map_or(0, |i| i + 1);
            let indent = if closer { inner.saturating_sub(1) } else { inner };
            if prev.is_some() {
                out.push('\n');
            }
            let group_start = prev.is_none_or(|p| is_opener(&p.tok));
            push_lines(&mut out, &t.leading, group_start, closer, &INDENT.repeat(inner));
            out.push_str(&INDENT.repeat(indent));
            line_indent = indent;
        } else if let Some(p) = prev {
            out.push_str(space_between(&p.tok, &t.tok, is_opener(&p.tok) && closer));
        }
        out.push_str(t.text);
        if let Some(c) = t.trailing {
            out.push_str(c);
        }
        if is_opener(&t.tok) {
            open.push(line_indent);
        } else if closer {
            open.pop();
        }
        prev = Some(t);
    }
    if !toks.is_empty() {
        out.push('\n');
    }
    push_lines(&mut out, tail, toks.is_empty(), true, "");
    out
}

/// Writes blank and comment lines, with runs of blank lines collapsed to
/// one and dropped at the start or end of a group.
fn push_lines(out: &mut String, lines: &[Line], group_start: bool, group_end: bool, indent: &str) {
    let mut kept: Vec<Line> = Vec::new();
    for &line in lines {
        let skip = line == Line::Blank && (kept.last().map_or(group_start, |&l| l == Line::Blank));
        if !skip {
            kept.push(line);
        }
    }
    while group_end && kept.last() == Some(&Line::Blank) {
        kept.pop();
    }
    for line in kept {
        if let Line::Comment(c) = line {
            out.push_str(indent);
            out.push_str(c);
        }
        out.push('\n');
    }
}

/// The value with [`SORTED_FIELDS`] lists sorted, for comparing before and after.
fn normalized(value: Value) -> Value {
    fn sort_field(v: Value) -> Value {
        match v {
            Value::Array(mut items)
                if items
                    .iter()
                    .all(|i| matches!(i, Value::String(_) | Value::RelPath(_) | Value::Target(_))) =>
            {
                items.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
                Value::Array(items)
            }
            Value::RelPaths(mut p) => {
                p.sort();
                Value::RelPaths(p)
            }
            Value::Targets(mut t) => {
                t.sort_by(|a, b| a.target_path().cmp(b.target_path()));
                Value::Targets(t)
            }
            // `srcs = [...] + select(...)`: the literal part is sorted.
            Value::Concat((l, r)) => Value::Concat((Box::new(sort_field(*l)), Box::new(sort_field(*r)))),
            v => normalized(v),
        }
    }
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(normalized).collect()),
        Value::Object(mut obj) => {
            for (k, v) in obj.fields.iter_mut() {
                let taken = std::mem::replace(v, Value::Array(vec![]));
                *v = if SORTED_FIELDS.contains(&k.0.as_str()) {
                    sort_field(taken)
                } else {
                    normalized(taken)
                };
            }
            Value::Object(obj)
        }
        v => v,
    }
}

#[cfg(test)]
mod tests {
    use super::format;

    #[test]
    fn formats_spacing_indent_commas_and_sorted_lists() {
        let before = r#"# A library.
cc_static_library(
  name="greet",
  srcs = [ RelPath("src/b.cpp"),RelPath("src/a.cpp") ],
      deps = Targets([":zlib", ":fmt",]),
  private_defines = [
      "ZETA=1",   # last
      # comes first
      "ALPHA=1"


  ],
  compiler_flags = ["-O2" , "-g"]+select(
     (target_platform,) => {
        (windows) = ["-DWIN"],
        default=[]
     }
  )
)
"#;
        let after = r#"# A library.
cc_static_library(
    name = "greet",
    srcs = [ RelPath("src/a.cpp"), RelPath("src/b.cpp") ],
    deps = Targets([ ":fmt", ":zlib" ]),
    private_defines = [
        "ZETA=1",   # last
        # comes first
        "ALPHA=1",
    ],
    compiler_flags = [ "-O2", "-g" ] + select(
        (target_platform) => {
            (windows) = [ "-DWIN" ],
            default = [],
        }
    ),
)
"#;
        assert_eq!(format(before).unwrap(), after);
        assert_eq!(format(after).unwrap(), after);
    }

    #[test]
    fn sorts_within_blank_line_runs_and_keeps_order_dependent_fields() {
        let src = "x(\n    srcs = [\n        \"b\", # bee\n        # about a\n        \"a\",\n\n        \"d\",\n        \"c\",\n    ],\n    include_dirs = [\"b\", \"a\"],\n)\n";
        let want = "x(\n    srcs = [\n        # about a\n        \"a\",\n        \"b\", # bee\n\n        \"c\",\n        \"d\",\n    ],\n    include_dirs = [ \"b\", \"a\" ],\n)\n";
        assert_eq!(format(src).unwrap(), want);
    }

    #[test]
    fn comment_only_file_is_idempotent() {
        assert_eq!(format("# note\n").unwrap(), "# note\n");
    }

    #[test]
    fn keeps_crlf_line_endings() {
        assert_eq!(format("x(\r\n  a=1\r\n)\r\n").unwrap(), "x(\r\n    a = 1,\r\n)\r\n");
    }

    #[test]
    fn keeps_comments_at_end_of_file_without_newline() {
        assert_eq!(
            format("x(a = 1) # one\n\n# two").unwrap(),
            "x(a = 1) # one\n\n# two\n"
        );
    }

    #[test]
    fn checked_in_anubis_files_are_formatted() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for rel in [
            "mode/ANUBIS",
            "toolchains/ANUBIS",
            "samples/basic/nasm_c/ANUBIS",
            "samples/external/ffmpeg/ANUBIS",
        ] {
            let src = std::fs::read_to_string(root.join(rel)).unwrap();
            assert_eq!(format(&src).unwrap(), src, "{rel} needs `anubis fmt`");
        }
    }
}
