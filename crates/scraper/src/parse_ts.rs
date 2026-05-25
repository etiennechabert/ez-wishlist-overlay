//! Extract `export const X = { ... }` object literals from upstream TypeScript
//! files and parse them as JSON5.
//!
//! The upstream repo (ExfilZone Assistant) stores its hideout/tasks data as
//! TypeScript object literals. We don't want a full TS toolchain in our build
//! pipeline, so we extract the literal text and hand it to a JSON5 parser
//! (which tolerates unquoted keys, trailing commas, and `//` comments).

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Find and parse the object literal assigned to `binding` in `source`.
///
/// Looks for `export const <binding>... = { ... }` and returns the parsed
/// value. Type annotations (`: Foo`) and trailing `as const` / `satisfies ...`
/// are ignored.
pub fn extract_object(source: &str, binding: &str) -> Result<Value> {
    let start_marker = format!("export const {binding}");
    let start = source
        .find(&start_marker)
        .with_context(|| format!("could not find `{start_marker}` in source"))?;

    // Find the first `=` after the binding name.
    let eq_offset = source[start..]
        .find('=')
        .with_context(|| format!("no `=` after `{start_marker}`"))?;
    let after_eq = start + eq_offset + 1;

    // Skip whitespace/comments until we find the opening `{`.
    let rest = &source[after_eq..];
    let open_rel = find_first_brace(rest)
        .with_context(|| format!("no opening `{{` after `{start_marker} =`"))?;
    let open_abs = after_eq + open_rel;

    // Scan forward, balanced, respecting strings and comments.
    let close_abs = find_matching_brace(source, open_abs)
        .with_context(|| format!("unbalanced braces in `{binding}` literal"))?;

    let literal = &source[open_abs..=close_abs];
    json5::from_str(literal).with_context(|| format!("json5 parse failed for `{binding}`"))
}

fn find_first_brace(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => return Some(i),
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                // Line comment.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // Block comment.
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

/// Given the position of an opening `{` in `source`, find the matching `}`.
///
/// Tracks string literals (single, double, backtick) and comments so braces
/// inside them don't count. Returns the byte offset of the matching close.
fn find_matching_brace(source: &str, open_at: usize) -> Result<usize> {
    let bytes = source.as_bytes();
    if bytes.get(open_at) != Some(&b'{') {
        bail!("expected `{{` at offset {open_at}");
    }

    let mut depth: i32 = 0;
    let mut i = open_at;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(i);
                }
                i += 1;
            }
            b'/' if peek_eq(bytes, i + 1, b'/') => i = skip_line_comment(bytes, i),
            b'/' if peek_eq(bytes, i + 1, b'*') => i = skip_block_comment(bytes, i),
            b'"' | b'\'' | b'`' => i = skip_string(bytes, i, b),
            _ => i += 1,
        }
    }
    bail!("ran off end of source before closing `{{` at offset {open_at}")
}

fn peek_eq(bytes: &[u8], i: usize, expected: u8) -> bool {
    bytes.get(i) == Some(&expected)
}

fn skip_line_comment(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(bytes: &[u8], mut i: usize) -> usize {
    i += 2;
    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
        i += 1;
    }
    i + 2
}

fn skip_string(bytes: &[u8], mut i: usize, quote: u8) -> usize {
    i += 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_simple_object() {
        let src = r#"
            export const foo = {
                "a": 1,
                "b": 2,
            } as const;
        "#;
        let v = extract_object(src, "foo").unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn handles_type_annotation_and_unquoted_keys() {
        let src = r#"
            export const corps: Record<string, Corp> = {
                "ark": { name: "ARK", icon: "/x" },
                "ntg": { name: "N.T.G", icon: "/y" },
            } as const;
        "#;
        let v = extract_object(src, "corps").unwrap();
        assert_eq!(v["ark"]["name"], "ARK");
        assert_eq!(v["ntg"]["icon"], "/y");
    }

    #[test]
    fn ignores_braces_in_strings_and_comments() {
        let src = r#"
            // {{{ this should not count
            export const x = {
                "s": "} not a brace {",
                /* { also fake } */
                "n": 1,
            };
        "#;
        let v = extract_object(src, "x").unwrap();
        assert_eq!(v["s"], "} not a brace {");
        assert_eq!(v["n"], 1);
    }
}
