//! `anubis fmt`: formats ANUBIS files from the token stream, not an AST.
//!
//! The lexer skips whitespace and comments, so they are exactly the text
//! between token spans. The formatter keeps that text's comments and the
//! author's line breaks, and normalizes the rest:
//!
//! - indentation: one level per line that opened a bracket still open;
//! - spacing within a line (`a = b`, `f(x)`, `[a, b]`, `{ a = b }`);
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

/// Fields whose list order doesn't matter. Include and library dirs and flags
/// are never sorted, nor are lists inside a `select`.
pub const SORTED_FIELDS: &[&str] = &[
    "srcs",
    "deps",
    "libraries",
    "public_libraries",
    "compiler_defines",
    "public_defines",
    "private_defines",
];

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
    let out = emit(&toks, &tail);

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
/// lists of plain strings for fields in [`SORTED_FIELDS`].
fn sort_lists(toks: &mut [Tok], pairs: &[usize]) {
    for c in 0..toks.len() {
        if toks[c].tok != Token::BracketClose {
            continue;
        }
        let o = pairs[c];
        let field_at = match (
            o.checked_sub(1).map(|i| &toks[i].tok),
            o.checked_sub(2).map(|i| &toks[i].tok),
        ) {
            (Some(Token::Equals), _) => o.checked_sub(2),
            (Some(Token::ParenOpen), Some(Token::Targets | Token::RelPaths)) => o.checked_sub(4),
            _ => None,
        };
        let sortable_field = field_at
            .filter(|&f| toks[f + 1].tok == Token::Equals)
            .is_some_and(|f| matches!(toks[f].tok, Token::Identifier(name) if SORTED_FIELDS.contains(&name)));
        if !sortable_field || c == o + 1 {
            continue;
        }

        // Units: an item (`"x"`, `RelPath("x")` or `Target("x")`) and its comma,
        // which every item of a multi-line list has by now.
        let multiline = toks[c].newline_before;
        let mut units: Vec<Vec<Tok>> = Vec::new();
        for t in &toks[o + 1..c] {
            match units.last_mut() {
                Some(u) if u.last().is_some_and(|l| l.tok != Token::Comma) => u.push(t.clone()),
                _ => units.push(vec![t.clone()]),
            }
        }
        let item_len = |u: &[Tok]| u.len() - usize::from(u.last().is_some_and(|t| t.tok == Token::Comma));
        let well_formed = units.iter().all(|u| {
            let item = &u[..item_len(u)];
            let shape_ok = match item {
                [s] => matches!(s.tok, Token::String(_)),
                [f, p, s, q] => {
                    matches!(f.tok, Token::RelPath | Token::Target)
                        && p.tok == Token::ParenOpen
                        && matches!(s.tok, Token::String(_))
                        && q.tok == Token::ParenClose
                }
                _ => false,
            };
            let lines_ok = !multiline || (u[0].newline_before && u[1..].iter().all(|t| !t.newline_before));
            let comments_ok = multiline || u.iter().all(|t| t.trailing.is_none() && t.leading.is_empty());
            shape_ok && lines_ok && comments_ok
        });
        if !well_formed {
            continue;
        }
        let key = |u: &Vec<Tok>| u[item_len(u) - 1 - usize::from(item_len(u) == 4)].text.to_owned();

        if multiline {
            // Sort each run of items between blank lines on its own. A run's
            // blank line stays on top; comments above an item move with it.
            let mut start = 0;
            for i in 1..=units.len() {
                if i == units.len() || units[i][0].leading.contains(&Line::Blank) {
                    let blank = units[start][0].leading.first() == Some(&Line::Blank);
                    while units[start][0].leading.first() == Some(&Line::Blank) {
                        units[start][0].leading.remove(0);
                    }
                    units[start..i].sort_by_key(key);
                    if blank {
                        units[start][0].leading.insert(0, Line::Blank);
                    }
                    start = i;
                }
            }
        } else {
            // One line: sort the items, then put the commas back between them.
            let comma = units[0].last().filter(|t| t.tok == Token::Comma).cloned();
            for u in units.iter_mut() {
                let n = item_len(u);
                u.truncate(n);
            }
            units.sort_by_key(key);
            let n = units.len();
            for u in units.iter_mut().take(n - 1) {
                u.push(comma.clone().expect("a list of several items has a comma"));
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
        (_, Comma | ParenClose | BracketClose) => "",
        (ParenOpen | BracketOpen, _) => "",
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
            let after_opener = prev.is_some_and(|p| is_opener(&p.tok));
            let mut lines: Vec<Line> = Vec::new();
            for &line in &t.leading {
                let at_group_start = lines.is_empty() && (after_opener || prev.is_none());
                if line == Line::Blank && (at_group_start || lines.last() == Some(&Line::Blank)) {
                    continue;
                }
                lines.push(line);
            }
            while closer && lines.last() == Some(&Line::Blank) {
                lines.pop();
            }
            if prev.is_some() {
                out.push('\n');
            }
            for line in lines {
                if let Line::Comment(c) = line {
                    out.push_str(&INDENT.repeat(inner));
                    out.push_str(c);
                }
                out.push('\n');
            }
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
    let mut blank = false;
    out.push('\n');
    for line in tail {
        match line {
            Line::Blank => blank = true,
            Line::Comment(c) => {
                if blank {
                    out.push('\n');
                }
                blank = false;
                out.push_str(c);
                out.push('\n');
            }
        }
    }
    out
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
  public_defines = [
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
    srcs = [RelPath("src/a.cpp"), RelPath("src/b.cpp")],
    deps = Targets([":fmt", ":zlib"]),
    public_defines = [
        # comes first
        "ALPHA=1",
        "ZETA=1",   # last
    ],
    compiler_flags = ["-O2", "-g"] + select(
        (target_platform) => {
            (windows) = ["-DWIN"],
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
        let src = "x(\n    srcs = [\n        \"b\",\n        \"a\",\n\n        \"d\",\n        \"c\",\n    ],\n    include_dirs = [\"b\", \"a\"],\n)\n";
        let want = "x(\n    srcs = [\n        \"a\",\n        \"b\",\n\n        \"c\",\n        \"d\",\n    ],\n    include_dirs = [\"b\", \"a\"],\n)\n";
        assert_eq!(format(src).unwrap(), want);
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
