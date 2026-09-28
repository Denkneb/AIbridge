//! Minimal command-policy primitives for simple shell commands (task 4.1).
//!
//! This crate reproduces only the tokenization and leading-assignment layer of
//! the reference Python `command_policy` module
//! (`/home/denis/Python/agent_bridge/src/agent_bridge/command_policy.py`,
//! functions `basename`, `is_assignment`, `split_command` and
//! `leading_assignments`). It deliberately makes **no** policy decision:
//! forbidden Git writes and wrappers (task 4.2), fail-closed compound shell
//! syntax, raw shell metacharacters, globs, shell executables and permission
//! decisions (task 4.3) are out of scope and are not implemented here.
//!
//! [`split_command`] is a POSIX tokenizer equivalent to Python
//! `shlex.split(text, posix=True)` with the default `comments=False` and
//! `whitespace_split=True` settings. The `shlex` crate is *not* used because it
//! diverges from the Python reference on two observable points: it treats `#`
//! as a comment introducer (Python keeps it as an ordinary character) and it
//! treats a backslash before a newline outside quotes as a line continuation
//! (Python keeps the newline). The tokenizer therefore implements the Python
//! state machine directly. As a fail-closed extension, a NUL byte is rejected
//! even though Python `shlex` would keep it: the reference command policy
//! rejects NUL before tokenizing, and a NUL byte can never form a valid command
//! token.
//!
//! Errors ([`TokenizeError`]) carry no payload, so a failed parse never includes
//! the original command or any of its tokens.

use std::error::Error;
use std::fmt;

/// Returns the package name as a trivial smoke-check helper.
#[must_use]
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

/// Why a command string could not be tokenized.
///
/// The error carries no payload, so it never leaks the original command or any
/// of its tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TokenizeError {
    /// The command contains a NUL byte, which can never form a valid token.
    Nul,
    /// The command ended while a single or double quotation was still open.
    UnmatchedQuote,
    /// The command ended immediately after an unescaped backslash.
    TrailingBackslash,
}

impl fmt::Display for TokenizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nul => f.write_str("command contains a NUL byte"),
            Self::UnmatchedQuote => f.write_str("command has an unmatched quotation"),
            Self::TrailingBackslash => f.write_str("command ends with a trailing backslash"),
        }
    }
}

impl Error for TokenizeError {}

/// Returns the final path component of `token`, mirroring the reference
/// `basename` (`token.rsplit("/", 1)[-1]`).
///
/// An empty token and a token that is exactly `/` both yield `""`.
#[must_use]
pub fn basename(token: &str) -> &str {
    match token.rsplit_once('/') {
        Some((_, name)) => name,
        None => token,
    }
}

/// Returns the `NAME`/`value` halves of a `NAME=value` assignment.
///
/// `NAME` must be non-empty, start with an alphabetic character or `_` and
/// contain only alphanumeric characters or `_` afterwards. The value may be
/// empty and may itself contain `=`.
fn assignment_parts(token: &str) -> Option<(&str, &str)> {
    let (name, value) = token.split_once('=')?;
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_alphabetic() || first == '_' => {
            if chars.all(|ch| ch.is_alphanumeric() || ch == '_') {
                Some((name, value))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Returns whether `token` is a `NAME=value` assignment, mirroring the
/// reference `is_assignment` contract.
#[must_use]
pub fn is_assignment(token: &str) -> bool {
    assignment_parts(token).is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Whitespace,
    Word,
    Quote(char),
    Escape,
}

fn is_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\r' | '\n')
}

/// Tokenizes a command string with Python `shlex.split(text, posix=True)`
/// semantics.
///
/// Quotes are removed, quoted whitespace is preserved, a backslash escapes the
/// next character outside single quotes, and adjacent quoted/unquoted
/// fragments are concatenated into one token. `#` is an ordinary character and
/// `\r`/`\n`/`\t`/space separate tokens. Empty and whitespace-only input yield
/// an empty vector.
///
/// # Errors
///
/// Returns [`TokenizeError::Nul`] for a NUL byte,
/// [`TokenizeError::UnmatchedQuote`] for an unterminated quotation and
/// [`TokenizeError::TrailingBackslash`] for input that ends with an unescaped
/// backslash. The error never contains the input.
pub fn split_command(text: &str) -> Result<Vec<String>, TokenizeError> {
    if text.contains('\0') {
        return Err(TokenizeError::Nul);
    }

    let mut tokens: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut state = State::Whitespace;
    let mut escaped_from = State::Whitespace;

    for ch in text.chars() {
        match state {
            State::Whitespace => {
                if is_whitespace(ch) {
                    if !token.is_empty() || quoted {
                        tokens.push(std::mem::take(&mut token));
                        quoted = false;
                    }
                } else if ch == '\\' {
                    escaped_from = State::Word;
                    state = State::Escape;
                } else if ch == '\'' || ch == '"' {
                    state = State::Quote(ch);
                } else {
                    token.push(ch);
                    state = State::Word;
                }
            }
            State::Quote(quote) => {
                quoted = true;
                if ch == quote {
                    state = State::Word;
                } else if quote == '"' && ch == '\\' {
                    escaped_from = State::Quote('"');
                    state = State::Escape;
                } else {
                    token.push(ch);
                }
            }
            State::Escape => {
                if let State::Quote(quote) = escaped_from
                    && ch != '\\'
                    && ch != quote
                {
                    token.push('\\');
                }
                token.push(ch);
                state = escaped_from;
            }
            State::Word => {
                if is_whitespace(ch) {
                    tokens.push(std::mem::take(&mut token));
                    quoted = false;
                    state = State::Whitespace;
                } else if ch == '\'' || ch == '"' {
                    state = State::Quote(ch);
                } else if ch == '\\' {
                    escaped_from = State::Word;
                    state = State::Escape;
                } else {
                    token.push(ch);
                }
            }
        }
    }

    match state {
        State::Whitespace => {}
        State::Word => tokens.push(token),
        State::Quote(_) => return Err(TokenizeError::UnmatchedQuote),
        State::Escape => return Err(TokenizeError::TrailingBackslash),
    }

    Ok(tokens)
}

/// Leading `NAME=value` assignments separated from the remaining argv.
///
/// Variables keep the order in which their names were first seen and a repeated
/// name keeps the last value, mirroring the reference `leading_assignments`
/// dictionary semantics.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LeadingAssignments {
    variables: Vec<(String, String)>,
    argv: Vec<String>,
}

impl LeadingAssignments {
    /// Returns the leading assignments in first-seen name order.
    #[must_use]
    pub fn variables(&self) -> &[(String, String)] {
        &self.variables
    }

    /// Returns the last value assigned to `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.variables
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Returns the argv that follows the leading assignments.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }
}

/// Splits leading `NAME=value` assignments from the remaining argv, mirroring
/// the reference `leading_assignments`.
///
/// Only the assignments before the first non-assignment token are consumed.
/// The caller is expected to have validated the token list first; this function
/// performs no policy decision.
#[must_use]
pub fn leading_assignments(tokens: &[String]) -> LeadingAssignments {
    let mut variables: Vec<(String, String)> = Vec::new();
    let mut index = 0;

    while let Some(token) = tokens.get(index) {
        let Some((name, value)) = assignment_parts(token) else {
            break;
        };
        if let Some(slot) = variables.iter_mut().find(|(key, _)| key == name) {
            slot.1 = value.to_owned();
        } else {
            variables.push((name.to_owned(), value.to_owned()));
        }
        index += 1;
    }

    LeadingAssignments {
        variables,
        argv: tokens[index..].to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LeadingAssignments, TokenizeError, basename, crate_name, is_assignment,
        leading_assignments, split_command,
    };

    fn split(text: &str) -> Vec<String> {
        split_command(text).expect("command should tokenize")
    }

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn crate_is_wired() {
        assert_eq!(crate_name(), "bridge-command-policy");
    }

    #[test]
    fn simple_command_is_split() {
        assert_eq!(split("ls -la"), owned(&["ls", "-la"]));
        assert_eq!(
            split(".venv/bin/pytest -q"),
            owned(&[".venv/bin/pytest", "-q"])
        );
        assert_eq!(
            split("/usr/bin/git status"),
            owned(&["/usr/bin/git", "status"])
        );
    }

    #[test]
    fn whitespace_is_collapsed_and_trimmed() {
        assert_eq!(split("  ls   -la  "), owned(&["ls", "-la"]));
        assert_eq!(split("a\tb"), owned(&["a", "b"]));
        assert_eq!(split("a\rb"), owned(&["a", "b"]));
        assert_eq!(split("a\nb"), owned(&["a", "b"]));
        assert_eq!(split("a \r\n\t b"), owned(&["a", "b"]));
    }

    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(split("echo 'a b'"), owned(&["echo", "a b"]));
        assert_eq!(split("echo 'a\\b'"), owned(&["echo", "a\\b"]));
        assert_eq!(split("echo '$HOME'"), owned(&["echo", "$HOME"]));
        assert_eq!(split("echo '#'"), owned(&["echo", "#"]));
    }

    #[test]
    fn double_quotes_are_literal_except_escapes() {
        assert_eq!(split("echo \"a b\""), owned(&["echo", "a b"]));
        assert_eq!(split("echo \"a\\\"b\""), owned(&["echo", "a\"b"]));
        assert_eq!(split("echo \"a\\\\b\""), owned(&["echo", "a\\b"]));
        assert_eq!(split("echo \"a\\$b\""), owned(&["echo", "a\\$b"]));
        assert_eq!(split("echo \"a\\nb\""), owned(&["echo", "a\\nb"]));
    }

    #[test]
    fn quoted_spaces_and_backslash_are_preserved() {
        assert_eq!(split("echo a\\ b"), owned(&["echo", "a b"]));
        assert_eq!(split("echo a\\$b"), owned(&["echo", "a$b"]));
        assert_eq!(split("echo a\\\tb"), owned(&["echo", "a\tb"]));
        assert_eq!(split("foo\\\nbar"), owned(&["foo\nbar"]));
    }

    #[test]
    fn adjacent_fragments_concatenate_into_one_token() {
        assert_eq!(split("a\"\"b"), owned(&["ab"]));
        assert_eq!(split("a''b"), owned(&["ab"]));
        assert_eq!(split("''"), owned(&[""]));
        assert_eq!(split("\"\""), owned(&[""]));
    }

    #[test]
    fn hash_is_an_ordinary_character() {
        assert_eq!(split("foo #bar"), owned(&["foo", "#bar"]));
        assert_eq!(split("foo#bar"), owned(&["foo#bar"]));
        assert_eq!(
            split("git log # comment"),
            owned(&["git", "log", "#", "comment"])
        );
    }

    #[test]
    fn path_prefixed_executable_basename() {
        assert_eq!(basename("/usr/bin/git"), "git");
        assert_eq!(basename("git"), "git");
        assert_eq!(basename("a/b/c"), "c");
        assert_eq!(basename(".venv/bin/pytest"), "pytest");
        assert_eq!(basename("/"), "");
        assert_eq!(basename(""), "");
    }

    #[test]
    fn valid_assignments_are_recognized() {
        for token in ["FOO=bar", "FOO=", "_FOO=bar", "A_b=1", "a=b=c", "Ünicode=1"] {
            assert!(is_assignment(token), "{token:?} should be an assignment");
        }
    }

    #[test]
    fn invalid_assignments_are_rejected() {
        for token in ["=bar", "1FOO=bar", "A-b=1", "FOO", "", "= ", "FOO BAR=1"] {
            assert!(
                !is_assignment(token),
                "{token:?} should not be an assignment"
            );
        }
    }

    #[test]
    fn leading_assignments_split_from_argv() {
        let result = leading_assignments(&owned(&["FOO=bar", ".venv/bin/pytest", "-q"]));
        assert_eq!(result.variables(), &[("FOO".to_owned(), "bar".to_owned())]);
        assert_eq!(result.get("FOO"), Some("bar"));
        assert_eq!(result.argv(), &owned(&[".venv/bin/pytest", "-q"]));

        let result = leading_assignments(&owned(&["A=1", "B=two", ".venv/bin/pytest", "-q"]));
        assert_eq!(
            result.variables(),
            &[
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "two".to_owned()),
            ]
        );
        assert_eq!(result.argv(), &owned(&[".venv/bin/pytest", "-q"]));
    }

    #[test]
    fn repeated_assignments_keep_first_order_and_last_value() {
        let result = leading_assignments(&owned(&["FOO=1", "BAR=2", "FOO=3", "cmd"]));
        assert_eq!(
            result.variables(),
            &[
                ("FOO".to_owned(), "3".to_owned()),
                ("BAR".to_owned(), "2".to_owned()),
            ]
        );
        assert_eq!(result.get("FOO"), Some("3"));
        assert_eq!(result.get("BAR"), Some("2"));
        assert_eq!(result.argv(), &owned(&["cmd"]));
    }

    #[test]
    fn assignments_after_the_executable_are_not_leading() {
        let result = leading_assignments(&owned(&["cmd", "FOO=1"]));
        assert!(result.variables().is_empty());
        assert_eq!(result.argv(), &owned(&["cmd", "FOO=1"]));
    }

    #[test]
    fn assignment_only_input_has_empty_argv() {
        let result = leading_assignments(&owned(&["FOO=bar"]));
        assert_eq!(result.variables(), &[("FOO".to_owned(), "bar".to_owned())]);
        assert!(result.argv().is_empty());
    }

    #[test]
    fn empty_and_whitespace_input_yield_no_tokens() {
        assert_eq!(split(""), owned(&[]));
        assert_eq!(split("   "), owned(&[]));
        assert_eq!(split("\t\r\n"), owned(&[]));

        let result = leading_assignments(&[]);
        assert_eq!(result, LeadingAssignments::default());
        assert!(result.variables().is_empty());
        assert!(result.argv().is_empty());
    }

    #[test]
    fn nul_is_rejected() {
        assert_eq!(split_command("echo\0"), Err(TokenizeError::Nul));
        assert_eq!(split_command("\0"), Err(TokenizeError::Nul));
    }

    #[test]
    fn unmatched_quotes_are_rejected() {
        assert_eq!(
            split_command("echo 'unclosed"),
            Err(TokenizeError::UnmatchedQuote)
        );
        assert_eq!(
            split_command("echo \"unclosed"),
            Err(TokenizeError::UnmatchedQuote)
        );
        assert_eq!(split_command("'"), Err(TokenizeError::UnmatchedQuote));
    }

    #[test]
    fn trailing_backslash_is_rejected() {
        assert_eq!(
            split_command("echo a\\"),
            Err(TokenizeError::TrailingBackslash)
        );
        assert_eq!(
            split_command("\"a\\"),
            Err(TokenizeError::TrailingBackslash)
        );
    }

    #[test]
    fn errors_do_not_reveal_the_input() {
        let secret = "echo super-secret-value\\";
        let error = split_command(secret).expect_err("trailing backslash should fail");
        let rendered = error.to_string();
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("echo"));
    }
}
