use ruff_python_trivia::leading_indentation;
use ruff_source_file::UniversalNewlines;
use ruff_text_size::{TextRange, TextSize};

use super::rst::is_field_list_marker;

/// Collects docstring lines without their universal-newline terminators while preserving their
/// source ranges.
///
/// For example, `first\r\nsecond` yields `first` at offset 0 and `second` at offset 7.
pub(super) fn parsed_lines(source: &str) -> Vec<ParsedLine<'_>> {
    source
        .universal_newlines()
        .map(|line| ParsedLine {
            text: line.as_str(),
            range: line.range(),
            indent: indentation(line.as_str()),
        })
        .collect()
}

/// A docstring line and its source range, excluding the newline terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ParsedLine<'a> {
    /// The line text, excluding its newline terminator.
    pub(super) text: &'a str,
    /// The byte range of `text` within the source document.
    pub(super) range: TextRange,
    /// The indentation in the source document.
    pub(super) indent: TextSize,
}

/// Returns whether `line` starts with a `CommonMark` list-item marker.
///
/// `CommonMark` limits ordered-list markers to nine digits to avoid integer
/// overflow in browsers: <https://spec.commonmark.org/0.31.2/#list-items>.
pub(in crate::docstring) fn starts_with_markdown_list_item(line: &str) -> bool {
    let bytes = line.as_bytes();
    if matches!(bytes, [b'-' | b'+' | b'*', b' ' | b'\t', ..]) {
        return true;
    }

    let digits = bytes
        .iter()
        .take(9)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    digits > 0
        && matches!(bytes.get(digits), Some(b'.' | b')'))
        && matches!(bytes.get(digits + 1), Some(b' ' | b'\t'))
}

/// Returns whether `text` consists of a complete Markdown code span.
///
/// For example, this returns `true` for ``"`value`"`` and `false` for
/// ``"`value` trailing"``.
pub(in crate::docstring) fn is_markdown_code_span(text: &str) -> bool {
    find_backtick_run(text, TextSize::ZERO).and_then(|opening| markdown_code_span(text, opening))
        == Some(TextRange::up_to(TextSize::of(text)))
}

/// Returns the byte range of the first consecutive backtick run at or after `from`.
///
/// For example, searching ``"value `code`"`` from the start returns the range covering the
/// opening ``"`"``.
pub(in crate::docstring) fn find_backtick_run(text: &str, from: TextSize) -> Option<TextRange> {
    let from = from.to_usize();
    let start = from + text.get(from..)?.find('`')?;
    let len = text[start..]
        .bytes()
        .take_while(|byte| *byte == b'`')
        .count();
    Some(TextRange::new(
        TextSize::of(&text[..start]),
        TextSize::of(&text[..start + len]),
    ))
}

/// Returns the Markdown code span delimited by `opening`, if it has a matching closing run.
///
/// For example, the opening run in "``value`with:ticks`` trailing" produces the range covering
/// "``value`with:ticks``".
pub(in crate::docstring) fn markdown_code_span(
    text: &str,
    opening: TextRange,
) -> Option<TextRange> {
    let mut search_from = opening.end();
    loop {
        let closing = find_backtick_run(text, search_from)?;
        if closing.len() == opening.len() {
            return Some(opening.cover(closing));
        }
        search_from = closing.end();
    }
}

/// Returns whether the backtick run at `index` is escaped by a preceding backslash.
///
/// For example, the backtick in ``"\`"`` is escaped, while the backtick in ``"\\`"`` is not.
pub(in crate::docstring) fn is_backtick_run_escaped(text: &str, index: usize) -> bool {
    !text[..index]
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'\\')
        .count()
        .is_multiple_of(2)
}

/// Returns the byte index after a Markdown code span, or after its opening backtick run when the
/// span is unclosed.
///
/// For example, ``"`value` trailing"`` advances past ``"`value`"``, while ``"`value"``
/// advances past only its opening backtick.
fn index_after_code_span_or_opening_run(text: &str, index: usize) -> Option<usize> {
    let opening = find_backtick_run(text, TextSize::of(&text[..index]))?;
    let span = markdown_code_span(text, opening).unwrap_or(opening);
    Some(span.end().to_usize())
}

/// Returns the end of an indented Markdown or reStructuredText container block.
///
/// For example, in `["- item", "  first", "  second", "next"]`, the block at index 0 ends
/// at index 3.
pub(super) fn container_block_end<T>(
    lines: &[T],
    index: usize,
    line_parts: impl Fn(&T) -> (&str, TextSize),
) -> Option<usize> {
    let (marker_text, marker_indent) = line_parts(lines.get(index)?);
    if !is_rest_directive_marker(marker_text)
        && !is_field_list_marker(marker_text)
        && !starts_with_markdown_list_item(marker_text.trim_start())
    {
        return None;
    }

    Some(
        (index + 1..lines.len())
            .find(|&end| {
                let (text, indent) = line_parts(&lines[end]);
                !text.trim().is_empty() && indent <= marker_indent
            })
            .unwrap_or(lines.len()),
    )
}

fn is_rest_directive_marker(line: &str) -> bool {
    let Some(directive) = line.trim_start().strip_prefix(".. ") else {
        return false;
    };
    let Some((name, _)) = directive.split_once("::") else {
        return false;
    };

    !name.is_empty() && !name.chars().any(char::is_whitespace)
}

/// Splits at the first top-level colon, ignoring colons inside brackets, quoted strings, and
/// Markdown code spans.
///
/// For example, `"value (Literal['a:b']): description"` splits before `"description"`, not at
/// the colon inside the quoted string.
///
/// If square or curly brackets are unclosed, falls back to the first colon outside parentheses.
/// This preserves item parsing for malformed type annotations.
pub(super) fn split_once_at_top_level_colon(line: &str) -> Option<(&str, &str)> {
    let mut nesting = BracketNesting::default();
    let mut fallback_colon = None;
    let mut index = 0;

    while index < line.len() {
        let character = line[index..].chars().next()?;
        match character {
            '\'' | '"' => {
                index = quoted_string_end(line, index, character);
                continue;
            }
            '`' if !is_backtick_run_escaped(line, index) => {
                index = index_after_code_span_or_opening_run(line, index)?;
                continue;
            }
            ':' if nesting.is_top_level() => return Some(split_at_colon(line, index)),
            ':' if nesting.is_outside_parentheses() => {
                fallback_colon.get_or_insert(index);
            }
            _ => nesting.update(character),
        }
        index += character.len_utf8();
    }

    fallback_colon.map(|index| split_at_colon(line, index))
}

#[derive(Default)]
struct BracketNesting {
    parentheses: usize,
    square: usize,
    curly: usize,
}

impl BracketNesting {
    fn is_top_level(&self) -> bool {
        self.parentheses == 0 && self.square == 0 && self.curly == 0
    }

    fn is_outside_parentheses(&self) -> bool {
        self.parentheses == 0
    }

    /// Updates the nesting depth while tolerating unmatched closing brackets.
    ///
    /// For example, `'('` increments the parenthesis depth and a later `')'` decrements it.
    fn update(&mut self, character: char) {
        match character {
            '(' => self.parentheses += 1,
            ')' => self.parentheses = self.parentheses.saturating_sub(1),
            '[' => self.square += 1,
            ']' => self.square = self.square.saturating_sub(1),
            '{' => self.curly += 1,
            '}' => self.curly = self.curly.saturating_sub(1),
            _ => {}
        }
    }
}

fn quoted_string_end(source: &str, start: usize, quote: char) -> usize {
    let content_start = start + quote.len_utf8();
    let mut escaped = false;
    for (offset, character) in source[content_start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
        } else if character == quote {
            return content_start + offset + character.len_utf8();
        }
    }
    source.len()
}

fn split_at_colon(line: &str, index: usize) -> (&str, &str) {
    (&line[..index], &line[index + ':'.len_utf8()..])
}

/// Splits and trims the prefix and contents of a trailing parenthetical expression.
///
/// Parentheses inside quoted strings and Markdown code spans do not affect nesting.
///
/// For example, `"value (Callable[[int], str])"` splits into `"value"` and
/// `"Callable[[int], str]"`.
pub(super) fn split_trailing_parenthetical(value: &str) -> Option<(&str, &str)> {
    if !value.ends_with(')') {
        return None;
    }

    let mut depth = 0usize;
    let mut outermost_opening = None;
    let mut index = 0;

    while index < value.len() {
        let character = value[index..].chars().next()?;
        match character {
            '\'' | '"' => {
                index = quoted_string_end(value, index, character);
                continue;
            }
            '`' if !is_backtick_run_escaped(value, index) => {
                index = index_after_code_span_or_opening_run(value, index)?;
                continue;
            }
            '(' => {
                if depth == 0 {
                    outermost_opening = Some(index);
                }
                depth += 1;
            }
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 && index + character.len_utf8() == value.len() {
                    let opening = outermost_opening?;
                    let prefix = value[..opening].trim();
                    let contents = value[opening + '('.len_utf8()..index].trim();
                    return Some((prefix, contents));
                }
            }
            _ => {}
        }
        index += character.len_utf8();
    }

    None
}

/// Calculates indentation width, advancing tabs to the next multiple of eight columns.
pub(super) fn indentation(line: &str) -> TextSize {
    TextSize::new(
        leading_indentation(line)
            .bytes()
            .fold(0u32, |column, byte| match byte {
                b'\t' => (column / 8 + 1) * 8,
                _ => column + 1,
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        is_markdown_code_span, split_once_at_top_level_colon, split_trailing_parenthetical,
    };

    #[test]
    fn recognizes_complete_markdown_code_spans() {
        for (text, expected) in [
            ("`value`", true),
            ("``value`with:ticks``", true),
            ("`value` trailing", false),
            ("before `value`", false),
            ("`first` second`", false),
            ("``value```", false),
            ("``", false),
            ("value", false),
        ] {
            assert_eq!(is_markdown_code_span(text), expected, "{text:?}");
        }
    }

    #[test]
    fn splits_after_nested_brackets() {
        assert_eq!(
            split_once_at_top_level_colon("value (dict[str, list[{key: value}]]): Description"),
            Some(("value (dict[str, list[{key: value}]])", " Description"))
        );
    }

    #[test]
    fn ignores_colons_inside_quoted_strings() {
        assert_eq!(
            split_once_at_top_level_colon(r"value (Literal['a\'b:c']): Description"),
            Some((r"value (Literal['a\'b:c'])", " Description"))
        );
    }

    #[test]
    fn ignores_colons_inside_code_spans() {
        assert_eq!(
            split_once_at_top_level_colon("value (`a:b`): Description"),
            Some(("value (`a:b`)", " Description"))
        );
    }

    #[test]
    fn distinguishes_escaped_backticks_from_code_spans() {
        assert_eq!(
            split_once_at_top_level_colon(r"value (\`): Description"),
            Some((r"value (\`)", " Description"))
        );
        assert_eq!(
            split_once_at_top_level_colon(r"value \\`a:b`: Description"),
            Some((r"value \\`a:b`", " Description"))
        );
    }

    #[test]
    fn treats_unmatched_backtick_runs_as_text() {
        assert_eq!(
            split_once_at_top_level_colon("value (`str): Description"),
            Some(("value (`str)", " Description"))
        );
    }

    #[test]
    fn matches_code_span_delimiter_length() {
        assert_eq!(
            split_once_at_top_level_colon("value (``a`b:c``): Description"),
            Some(("value (``a`b:c``)", " Description"))
        );
    }

    #[test]
    fn recovers_from_unclosed_square_brackets() {
        assert_eq!(
            split_once_at_top_level_colon("value [str: Description"),
            Some(("value [str", " Description"))
        );
    }

    #[test]
    fn does_not_recover_from_unclosed_parentheses() {
        assert_eq!(
            split_once_at_top_level_colon("value (str: Description"),
            None
        );
    }

    #[test]
    fn splits_trailing_parenthesized_group() {
        assert_eq!(
            split_trailing_parenthetical(" value  (  str  )"),
            Some(("value", "str"))
        );
    }

    #[test]
    fn splits_nested_parenthesized_group() {
        assert_eq!(
            split_trailing_parenthetical("value (Callable[(int), tuple[str]])"),
            Some(("value", "Callable[(int), tuple[str]]"))
        );
    }

    #[test]
    fn ignores_parentheses_inside_quoted_strings() {
        assert_eq!(
            split_trailing_parenthetical("value (Literal[')'])"),
            Some(("value", "Literal[')']"))
        );
    }

    #[test]
    fn ignores_parentheses_inside_code_spans() {
        assert_eq!(
            split_trailing_parenthetical("value (`(`)"),
            Some(("value", "`(`"))
        );
    }

    #[test]
    fn ignores_parentheses_after_escaped_quotes() {
        assert_eq!(
            split_trailing_parenthetical(r#"value (Literal["a\"b)c"])"#),
            Some(("value", r#"Literal["a\"b)c"]"#))
        );
    }

    #[test]
    fn rejects_unclosed_parenthesized_group() {
        assert_eq!(split_trailing_parenthetical("value (str"), None);
    }

    #[test]
    fn rejects_parenthesized_group_before_trailing_text() {
        assert_eq!(split_trailing_parenthetical("value (str) or None"), None);
    }
}
