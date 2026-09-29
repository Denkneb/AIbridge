//! Command-policy primitives for simple shell commands (tasks 4.1-4.3) and the
//! verifier-level test-command validation built on them (task 5.1).
//!
//! This crate reproduces the tokenization and leading-assignment layer (task
//! 4.1), the token-level forbidden-Git-write and wrapper layer (task 4.2) and
//! the fail-closed compound-shell-syntax layer (task 4.3) of the reference
//! Python `command_policy` module
//! (`/home/denis/Python/agent_bridge/src/agent_bridge/command_policy.py`):
//! `basename`, `is_assignment`, `split_command`, `leading_assignments`,
//! `has_shell_metacharacters`, `has_glob`, `git_invocation_problem`,
//! `env_invocation_problem`, the wrapper-aware `tokens_problem` and the
//! raw-pattern entry point `bash_pattern_problem`.
//!
//! Task 4.2 classifies forbidden `git add`/`commit`/`push` writes through
//! leading assignments, path-prefixed executables and the wrappers `env`,
//! `sudo`, `command`, `exec`, `nohup`, `nice` and `time`, and fails closed on
//! `env` command-splitting forms.
//!
//! Task 4.3 adds [`bash_pattern_problem`]: the raw pattern is scanned before
//! tokenization, so shell syntax that cannot be statically resolved (separators,
//! pipes, background execution, redirects, command substitution, backticks,
//! subshells, brace/variable expansion and newlines) fails closed as
//! `unprovable_shell_syntax` even when it appears inside quotes. Empty and
//! whitespace-only patterns yield `empty_bash_pattern`, NUL and untokenizable
//! input yield `unparsable_bash_pattern`, general globs yield
//! `unprovable_glob_command`, and shell executables (`sh`, `bash`, `zsh`,
//! `dash`, `ksh`, `fish`) are re-checked through `-c` or rejected as
//! `unsafe_shell_invocation`/`shell_command_missing`. `eval` joins its arguments
//! and re-checks them as a fresh pattern. Task 5.1 adds the narrow string API
//! [`validate_test_commands`]: each string is checked through
//! [`bash_pattern_problem`], and a command left with an empty argv after its
//! leading assignments is rejected as [`TestCommandReason::MissingExecutable`].
//! Non-string/non-list verifier inputs belong to configuration/transport and
//! remain outside this crate, as does the `permission`-context allow reason
//! `configured`.
//!
//! Reference semantics are reproduced exactly, including two observable
//! details: the raw scan happens before tokenization, so a metacharacter or glob
//! inside quotes is still seen; and a non-`-c` shell executable (for example
//! `bash --version`) fails closed as `unsafe_shell_invocation`.
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

/// Git subcommands that may write repository history.
const GIT_WRITE_SUBCOMMANDS: [&str; 3] = ["add", "commit", "push"];

/// Git global options that consume the following token as a separate value, so
/// the subcommand scan must skip that value instead of mistaking it for the
/// subcommand. Mirrors the reference `_GIT_VALUE_OPTIONS`.
const GIT_VALUE_OPTIONS: [&str; 9] = [
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
    "--super-prefix",
    "--shallow-file",
];

/// Command wrappers that execute the command named by their remaining argv.
/// Mirrors the reference `_SHELL_WRAPPERS`.
const SHELL_WRAPPERS: [&str; 7] = ["sudo", "env", "command", "exec", "nohup", "nice", "time"];

/// Shell executables whose script argument the policy must inspect. Mirrors the
/// reference `_SHELL_EXECUTABLES`.
const SHELL_EXECUTABLES: [&str; 6] = ["sh", "bash", "zsh", "dash", "ksh", "fish"];

/// Builtins that re-run their joined arguments as a fresh command. Mirrors the
/// reference `_SHELL_EVAL`.
const SHELL_EVAL: [&str; 1] = ["eval"];

/// All GNU `env` long options, used to resolve unambiguous abbreviations the
/// same way `getopt_long` does. Only `--split-string` can hide a fresh command.
const ENV_LONG_OPTIONS: [&str; 12] = [
    "split-string",
    "unset",
    "chdir",
    "ignore-environment",
    "null",
    "debug",
    "help",
    "version",
    "list-signal-handling",
    "block-signal",
    "default-signal",
    "ignore-signal",
];

/// Why a token-level command policy rejects an invocation.
///
/// The reason carries no payload, so a denied command never leaks its command
/// text, argv, paths or secrets through `Debug`/`Display`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PolicyReason {
    /// The resolved `git` subcommand is `add`, `commit` or `push`.
    GitWriteBlocked,
    /// The `git` subcommand token contains glob characters and cannot be
    /// resolved to a concrete subcommand.
    UnprovableGitGlob,
    /// A wrapper invocation cannot be proven safe: `env` is asked to split a
    /// command string (`-S`/`--split-string`, including combined/abbreviated
    /// spellings), or an `env` option is unknown, ambiguous or missing a value.
    UnprovableWrapperCommand,
    /// The raw pattern is empty or whitespace-only.
    EmptyBashPattern,
    /// The raw pattern contains shell metacharacters that cannot be statically
    /// resolved: separators, pipes, background execution, redirects, command
    /// substitution, backticks, subshells, brace/variable expansion or
    /// newlines. The scan runs before tokenization, so quoting does not hide it.
    UnprovableShellSyntax,
    /// A non-`git` token contains glob characters (`*`, `?`, `[`) and cannot be
    /// resolved to a concrete executable.
    UnprovableGlobCommand,
    /// A shell executable was invoked without `-c`, so its script cannot be
    /// inspected.
    UnsafeShellInvocation,
    /// A shell executable was invoked with `-c` but no following command string.
    ShellCommandMissing,
    /// The pattern contains a NUL byte or cannot be tokenized.
    UnparsableBashPattern,
}

impl PolicyReason {
    /// Returns the stable reason identifier, matching the reference
    /// `reason_category` strings for the categories implemented here.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitWriteBlocked => "git_write_blocked",
            Self::UnprovableGitGlob => "unprovable_git_glob",
            Self::UnprovableWrapperCommand => "unprovable_wrapper_command",
            Self::EmptyBashPattern => "empty_bash_pattern",
            Self::UnprovableShellSyntax => "unprovable_shell_syntax",
            Self::UnprovableGlobCommand => "unprovable_glob_command",
            Self::UnsafeShellInvocation => "unsafe_shell_invocation",
            Self::ShellCommandMissing => "shell_command_missing",
            Self::UnparsableBashPattern => "unparsable_bash_pattern",
        }
    }
}

impl fmt::Display for PolicyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed token-level policy decision.
///
/// The decision carries only a [`PolicyReason`] on denial and never the command
/// text, argv, paths or secrets, so it is safe to surface to later stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PolicyDecision {
    /// The invocation is a simple, statically provable read-only command.
    Allow,
    /// The invocation must be rejected; the reason is safe to display.
    Deny(PolicyReason),
}

impl PolicyDecision {
    /// Returns whether the decision is [`PolicyDecision::Allow`].
    #[must_use]
    pub fn is_allow(self) -> bool {
        matches!(self, Self::Allow)
    }

    /// Returns the denial reason, if any.
    #[must_use]
    pub fn reason(self) -> Option<PolicyReason> {
        match self {
            Self::Allow => None,
            Self::Deny(reason) => Some(reason),
        }
    }
}

/// Returns whether `value` contains a shell glob character (`*`, `?`, `[`),
/// mirroring the reference `has_glob`.
fn has_glob(value: &str) -> bool {
    value.chars().any(|ch| matches!(ch, '*' | '?' | '['))
}

/// Returns whether `value` contains raw shell syntax that cannot be statically
/// resolved, mirroring the reference `has_shell_metacharacters`.
///
/// The set is exactly the reference `_SHELL_METACHARACTERS`
/// (`; | & < > \` $ ( ) { }`) plus newline and carriage return. The check runs
/// on the raw pattern before tokenization, so a metacharacter inside quotes is
/// still detected.
fn has_shell_metacharacters(value: &str) -> bool {
    value.chars().any(|ch| {
        matches!(
            ch,
            ';' | '|' | '&' | '<' | '>' | '`' | '$' | '(' | ')' | '{' | '}' | '\n' | '\r'
        )
    })
}

/// Returns whether `base` (a basename) names a supported command wrapper.
fn is_shell_wrapper(base: &str) -> bool {
    SHELL_WRAPPERS.contains(&base)
}

/// Returns a problem reason when a `git` invocation may write history,
/// mirroring the reference `git_invocation_problem`.
///
/// Global options and their separate values are skipped before the first
/// positional token is treated as the subcommand. A glob in the subcommand
/// position cannot be resolved and fails closed.
fn git_invocation_problem(args: &[String]) -> Option<PolicyReason> {
    let mut index = 0;
    while let Some(token) = args.get(index) {
        if GIT_VALUE_OPTIONS.contains(&token.as_str()) {
            index += 2;
            continue;
        }
        if token.starts_with("--") && token.contains('=') {
            index += 1;
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        if has_glob(token) {
            return Some(PolicyReason::UnprovableGitGlob);
        }
        if GIT_WRITE_SUBCOMMANDS.contains(&basename(token).to_lowercase().as_str()) {
            return Some(PolicyReason::GitWriteBlocked);
        }
        return None;
    }
    None
}

/// How a single `env` option token advances the option scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvOption {
    /// A no-value or optional-value option: advance one token.
    Advance,
    /// A required-value option without an inline value: consume the next token.
    ValueNext,
    /// A command-splitting form that must fail closed.
    SplitString,
    /// An unknown, ambiguous or malformed option that must fail closed.
    Unknown,
}

/// Classifies a single `env` option token.
fn classify_env_option(opt: &str) -> EnvOption {
    if let Some(long) = opt.strip_prefix("--") {
        return classify_env_long_option(long);
    }
    if let Some(short) = opt.strip_prefix('-') {
        return classify_env_short_options(short);
    }
    EnvOption::Unknown
}

/// Classifies a long `env` option, resolving unambiguous abbreviations.
fn classify_env_long_option(long: &str) -> EnvOption {
    let (name, has_inline_value) = match long.split_once('=') {
        Some((name, _)) => (name, true),
        None => (long, false),
    };
    if name.is_empty() {
        return EnvOption::Unknown;
    }

    let mut resolved: Option<&str> = None;
    for candidate in ENV_LONG_OPTIONS {
        if candidate.starts_with(name) {
            if resolved.is_some() {
                return EnvOption::Unknown;
            }
            resolved = Some(candidate);
        }
    }

    match resolved {
        Some("split-string") => EnvOption::SplitString,
        Some("unset" | "chdir") => {
            if has_inline_value {
                EnvOption::Advance
            } else {
                EnvOption::ValueNext
            }
        }
        Some("block-signal" | "default-signal" | "ignore-signal") => EnvOption::Advance,
        Some(
            "ignore-environment" | "null" | "debug" | "help" | "version" | "list-signal-handling",
        ) => {
            if has_inline_value {
                EnvOption::Unknown
            } else {
                EnvOption::Advance
            }
        }
        _ => EnvOption::Unknown,
    }
}

/// Classifies a short `env` option cluster such as `-i`, `-u NAME` or `-iS`.
///
/// A combined cluster is walked character by character: `u`/`C` consume the
/// remainder of the cluster as their value, `S` is the command-splitting form,
/// and any unknown flag fails closed so a hidden `S` cannot slip through.
fn classify_env_short_options(short: &str) -> EnvOption {
    let mut chars = short.chars();
    while let Some(ch) = chars.next() {
        match ch {
            'i' | '0' | 'v' => {}
            'u' | 'C' => {
                return if chars.as_str().is_empty() {
                    EnvOption::ValueNext
                } else {
                    EnvOption::Advance
                };
            }
            'S' => return EnvOption::SplitString,
            _ => return EnvOption::Unknown,
        }
    }
    EnvOption::Advance
}

/// Fails closed when `env` is told to split a command string, mirroring and
/// hardening the reference `env_invocation_problem`.
///
/// `env -S`/`--split-string` re-parses its argument as a fresh command, so a
/// forbidden `git` write can hide inside a single whitespace-containing token
/// that ordinary tokenization would never see. The reference only recognizes
/// `-S` at the start of a token; this implementation also rejects combined
/// short-option clusters (`-iS`) and unambiguous long-option abbreviations
/// (`--split`), and fails closed on unknown or value-missing `env` options.
fn env_invocation_problem(tokens: &[String], start: usize) -> Option<PolicyReason> {
    let mut probe = start;
    while let Some(opt) = tokens.get(probe) {
        if opt == "--" {
            return None;
        }
        if is_assignment(opt) {
            probe += 1;
            continue;
        }
        if !opt.starts_with('-') {
            return None;
        }
        match classify_env_option(opt) {
            EnvOption::Advance => probe += 1,
            EnvOption::ValueNext => {
                if probe + 1 >= tokens.len() {
                    return Some(PolicyReason::UnprovableWrapperCommand);
                }
                probe += 2;
            }
            EnvOption::SplitString | EnvOption::Unknown => {
                return Some(PolicyReason::UnprovableWrapperCommand);
            }
        }
    }
    None
}

/// Returns a problem reason for a tokenized simple command, mirroring the
/// reference `tokens_problem`.
///
/// Leading (and any) `NAME=value` assignments are skipped, supported wrappers
/// are unwrapped so nested and path-prefixed invocations stay visible, shell
/// executables are unwrapped through a safe `-c` script or rejected, `eval`
/// re-checks its joined arguments as a fresh pattern, and the first `git`
/// invocation is classified. A general glob in an ordinary token fails closed.
///
/// This function classifies already-tokenized input; [`bash_pattern_problem`]
/// is the raw-pattern entry point that additionally rejects shell syntax before
/// tokenization.
#[must_use]
pub fn tokens_problem(tokens: &[String]) -> Option<PolicyReason> {
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        if is_assignment(token) {
            index += 1;
            continue;
        }
        let base = basename(token);
        if is_shell_wrapper(base) {
            if base == "env"
                && let Some(problem) = env_invocation_problem(tokens, index + 1)
            {
                return Some(problem);
            }
            index += 1;
            continue;
        }
        if SHELL_EXECUTABLES.contains(&base) {
            let mut probe = index + 1;
            while let Some(candidate) = tokens.get(probe) {
                if !candidate.starts_with('-') || candidate == "-c" {
                    break;
                }
                probe += 1;
            }
            if tokens.get(probe).map(String::as_str) == Some("-c") {
                let Some(script) = tokens.get(probe + 1) else {
                    return Some(PolicyReason::ShellCommandMissing);
                };
                let Ok(nested) = split_command(script) else {
                    return Some(PolicyReason::UnparsableBashPattern);
                };
                if let Some(problem) = tokens_problem(&nested) {
                    return Some(problem);
                }
                index = probe + 2;
                continue;
            }
            return Some(PolicyReason::UnsafeShellInvocation);
        }
        if base == "git" {
            if let Some(problem) = git_invocation_problem(&tokens[index + 1..]) {
                return Some(problem);
            }
            index += 1;
            continue;
        }
        if SHELL_EVAL.contains(&base) {
            // `eval` joins its arguments and runs them as another command, so
            // the joined text is re-checked as a fresh pattern. Like the
            // reference, `eval` is terminal: the rest of the argv is consumed.
            let joined = tokens[index + 1..].join(" ");
            if let Some(problem) = bash_pattern_problem(&joined) {
                return Some(problem);
            }
            return None;
        }
        if has_glob(token) {
            return Some(PolicyReason::UnprovableGlobCommand);
        }
        index += 1;
    }
    None
}

/// Returns the typed token-level policy decision for a tokenized command.
///
/// This is the narrow API for later stages: it never carries the command text,
/// argv, paths or secrets, only a [`PolicyReason`] on denial.
#[must_use]
pub fn policy_decision(tokens: &[String]) -> PolicyDecision {
    match tokens_problem(tokens) {
        Some(reason) => PolicyDecision::Deny(reason),
        None => PolicyDecision::Allow,
    }
}

/// Returns a problem reason when a raw shell pattern is unsafe to approve or
/// execute, mirroring the reference `bash_pattern_problem`.
///
/// The raw pattern is validated before tokenization, so shell syntax that cannot
/// be statically resolved fails closed as [`PolicyReason::UnprovableShellSyntax`]
/// even inside quotes. Empty/whitespace-only input yields
/// [`PolicyReason::EmptyBashPattern`], NUL or untokenizable input yields
/// [`PolicyReason::UnparsableBashPattern`], and the remaining tokens are
/// classified by [`tokens_problem`].
#[must_use]
pub fn bash_pattern_problem(pattern: &str) -> Option<PolicyReason> {
    if pattern.trim().is_empty() {
        return Some(PolicyReason::EmptyBashPattern);
    }
    if pattern.contains('\0') {
        return Some(PolicyReason::UnparsableBashPattern);
    }
    if has_shell_metacharacters(pattern) {
        return Some(PolicyReason::UnprovableShellSyntax);
    }
    match split_command(pattern) {
        Ok(tokens) => tokens_problem(&tokens),
        Err(_) => Some(PolicyReason::UnparsableBashPattern),
    }
}

/// Returns the typed policy decision for a raw shell pattern.
///
/// This is the narrow raw-pattern entry point for later stages: it never carries
/// the pattern, argv, paths or secrets, only a [`PolicyReason`] on denial.
#[must_use]
pub fn bash_pattern_decision(pattern: &str) -> PolicyDecision {
    match bash_pattern_problem(pattern) {
        Some(reason) => PolicyDecision::Deny(reason),
        None => PolicyDecision::Allow,
    }
}

/// Why a test command was rejected by verifier-level validation.
///
/// The reason carries no payload, so a rejected command never leaks its command
/// text, argv, paths or secrets through `Debug`/`Display`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TestCommandReason {
    /// The command failed the shared fail-closed bash/policy check; the wrapped
    /// [`PolicyReason`] is the reference policy category.
    Policy(PolicyReason),
    /// The command is only leading `NAME=value` assignments with no executable
    /// left for the verifier to run. Mirrors the reference category
    /// `missing_executable`, which exists only in the `test_command` context.
    MissingExecutable,
}

impl TestCommandReason {
    /// Returns the stable reason identifier, matching the reference
    /// `reason_category` strings for the categories implemented here.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Policy(reason) => reason.as_str(),
            Self::MissingExecutable => "missing_executable",
        }
    }
}

impl fmt::Display for TestCommandReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for TestCommandReason {}

/// One rejected test command: its position in the input list and a payload-free
/// reason.
///
/// The problem carries only the index and a [`TestCommandReason`], never the
/// command text, argv, paths or secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TestCommandProblem {
    index: usize,
    reason: TestCommandReason,
}

impl TestCommandProblem {
    /// Returns the zero-based position of the rejected command.
    #[must_use]
    pub fn index(self) -> usize {
        self.index
    }

    /// Returns the payload-free rejection reason.
    #[must_use]
    pub fn reason(self) -> TestCommandReason {
        self.reason
    }
}

/// Classifies one string test command, mirroring the per-command branch of the
/// reference `verifier.validate_test_commands`.
fn test_command_problem(command: &str) -> Option<TestCommandReason> {
    if let Some(reason) = bash_pattern_problem(command) {
        return Some(TestCommandReason::Policy(reason));
    }
    // The command already passed `bash_pattern_problem`, so tokenization cannot
    // fail here; fall back fail closed if it ever does.
    let tokens = match split_command(command) {
        Ok(tokens) => tokens,
        Err(_) => {
            return Some(TestCommandReason::Policy(
                PolicyReason::UnparsableBashPattern,
            ));
        }
    };
    let assignments = leading_assignments(&tokens);
    if assignments.argv().is_empty() {
        return Some(TestCommandReason::MissingExecutable);
    }
    None
}

/// Validates a list of test commands, mirroring the reference
/// `verifier.validate_test_commands`.
///
/// An empty list is valid and returns an empty vector. Each command is first
/// checked with the shared fail-closed [`bash_pattern_problem`]; a command that
/// passes the policy but consists only of leading `NAME=value` assignments is
/// rejected as [`TestCommandReason::MissingExecutable`]. Results keep the input
/// order and carry the index of each rejected command.
///
/// The returned problems carry only the index and a payload-free reason, never
/// the command text, argv, paths or secrets.
#[must_use]
pub fn validate_test_commands(commands: &[&str]) -> Vec<TestCommandProblem> {
    commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            test_command_problem(command).map(|reason| TestCommandProblem { index, reason })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        LeadingAssignments, PolicyDecision, PolicyReason, TestCommandProblem, TestCommandReason,
        TokenizeError, basename, bash_pattern_decision, bash_pattern_problem, crate_name,
        is_assignment, leading_assignments, policy_decision, split_command, tokens_problem,
        validate_test_commands,
    };

    fn split(text: &str) -> Vec<String> {
        split_command(text).expect("command should tokenize")
    }

    fn problem(command: &str) -> Option<PolicyReason> {
        tokens_problem(&split(command))
    }

    fn pattern_problem(command: &str) -> Option<PolicyReason> {
        bash_pattern_problem(command)
    }

    fn decision(command: &str) -> PolicyDecision {
        policy_decision(&split(command))
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

    #[test]
    fn read_only_git_invocations_are_allowed() {
        for command in [
            "git status",
            "git -C /repo status --short",
            "git log --oneline -5",
            "git diff --check",
            "/usr/bin/git status",
            "git --version",
            "git -c core.pager=cat status",
            "git --git-dir /repo/.git status",
        ] {
            assert_eq!(problem(command), None, "{command:?} should be allowed");
            assert!(
                decision(command).is_allow(),
                "{command:?} should be allowed"
            );
        }
    }

    #[test]
    fn git_writes_are_blocked() {
        for command in [
            "git add .",
            "git commit -m x",
            "git push origin main",
            "git    add .",
            "/usr/bin/git add .",
            "./git commit -m x",
            "git -C /repo commit -m x",
            "git -c user.name=x push",
            "git --git-dir=/repo/.git push",
            "git --git-dir /repo/.git push",
            "git --work-tree=/repo push",
            "git --namespace=x push",
            "git -C /repo -c x=y commit -m z",
            "git ADD .",
            "FOO=bar git push",
            "FOO=git git push",
        ] {
            assert_eq!(
                problem(command),
                Some(PolicyReason::GitWriteBlocked),
                "{command:?} should be blocked"
            );
        }
    }

    #[test]
    fn git_glob_subcommands_fail_closed() {
        for command in ["git *", "git ad*", "git -C /repo add*"] {
            assert_eq!(
                problem(command),
                Some(PolicyReason::UnprovableGitGlob),
                "{command:?} should fail closed"
            );
        }
    }

    #[test]
    fn supported_wrappers_do_not_hide_git_writes() {
        for command in [
            "sudo git push",
            "command git commit -m x",
            "nohup git push",
            "nice git push",
            "time git push",
            "exec git push",
            "env git add .",
            "/usr/bin/sudo /usr/bin/git push",
            "/bin/env git add .",
            "sudo -u root git push",
            "nice -n 10 git push",
        ] {
            assert_eq!(
                problem(command),
                Some(PolicyReason::GitWriteBlocked),
                "{command:?} should be blocked"
            );
        }
    }

    #[test]
    fn nested_wrappers_are_unwrapped() {
        for command in [
            "sudo env git push",
            "env sudo git push",
            "nohup nice git push",
            "/usr/bin/env /usr/bin/git add .",
            "env sudo -u root git commit -m x",
        ] {
            assert_eq!(
                problem(command),
                Some(PolicyReason::GitWriteBlocked),
                "{command:?} should be blocked"
            );
        }
    }

    #[test]
    fn safe_wrapper_invocations_are_allowed() {
        for command in [
            "sudo ls -la",
            "command ls -la",
            "env -i git status",
            "env FOO=bar git log",
            "env -u FOO git status",
            "env -C /repo git status",
            "env -- git status",
            "env --ignore-environment git status",
            "env --null git status",
            "env --debug git status",
            "env --block-signal=INT git status",
            "env -u -S 'git status'",
            "nohup ls",
            "nice ls",
            "time ls",
            "exec ls",
        ] {
            assert_eq!(problem(command), None, "{command:?} should be allowed");
        }
    }

    #[test]
    fn env_split_string_forms_fail_closed() {
        for command in [
            "env -S 'git push'",
            "env --split-string 'git add .'",
            "env --split-string='git push'",
            "env -i -S 'git commit -m x'",
            "env FOO=bar -S 'git push'",
            "env -Sgit push",
            "env -S 'git status'",
        ] {
            assert_eq!(
                problem(command),
                Some(PolicyReason::UnprovableWrapperCommand),
                "{command:?} should fail closed"
            );
        }
    }

    #[test]
    fn combined_and_abbreviated_env_split_options_fail_closed() {
        for command in [
            "env -iS 'git push'",
            "env -0S 'git push'",
            "env -vS 'git push'",
            "env --split 'git push'",
            "env --spl 'git push'",
            "env --s 'git push'",
        ] {
            assert_eq!(
                problem(command),
                Some(PolicyReason::UnprovableWrapperCommand),
                "{command:?} should fail closed"
            );
        }
    }

    #[test]
    fn malformed_or_unsupported_env_options_fail_closed() {
        for command in [
            "env -u",
            "env -C",
            "env --unset",
            "env --chdir",
            "env -iu",
            "env -xC",
            "env --unknown",
            "env --ignore- git status",
            "env --null=x git status",
        ] {
            assert_eq!(
                problem(command),
                Some(PolicyReason::UnprovableWrapperCommand),
                "{command:?} should fail closed"
            );
        }
    }

    #[test]
    fn leading_assignments_do_not_hide_git_writes() {
        assert_eq!(
            problem("FOO=bar git push"),
            Some(PolicyReason::GitWriteBlocked)
        );
        assert_eq!(problem("A=1 B=two .venv/bin/pytest -q"), None);
        assert_eq!(problem("FOO=bar"), None);
        assert_eq!(problem(""), None);
        assert!(decision("FOO=bar .venv/bin/pytest -q").is_allow());
    }

    #[test]
    fn wrapper_alone_without_command_is_not_a_git_write() {
        for command in [
            "sudo",
            "env",
            "env FOO=bar",
            "command",
            "nice",
            "time",
            "exec",
        ] {
            assert_eq!(problem(command), None, "{command:?} should not be blocked");
        }
    }

    #[test]
    fn decision_reason_matches_problem() {
        assert_eq!(
            decision("git push").reason(),
            Some(PolicyReason::GitWriteBlocked)
        );
        assert_eq!(decision("git status").reason(), None);
        assert!(decision("git status").is_allow());
        assert!(!decision("git push").is_allow());
    }

    #[test]
    fn policy_reason_strings_match_reference_categories() {
        assert_eq!(PolicyReason::GitWriteBlocked.as_str(), "git_write_blocked");
        assert_eq!(
            PolicyReason::UnprovableGitGlob.as_str(),
            "unprovable_git_glob"
        );
        assert_eq!(
            PolicyReason::UnprovableWrapperCommand.as_str(),
            "unprovable_wrapper_command"
        );
        assert_eq!(
            PolicyReason::GitWriteBlocked.to_string(),
            "git_write_blocked"
        );
    }

    #[test]
    fn decisions_and_reasons_do_not_reveal_tokens() {
        let command = "git push super-secret-token";
        let rendered = format!(
            "{:?} {:?} {}",
            decision(command),
            problem(command).expect("write should be blocked"),
            problem(command).expect("write should be blocked")
        );
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("super"));
        assert!(!rendered.contains("push"));
    }

    #[test]
    fn pattern_empty_and_whitespace_fail_closed() {
        for command in ["", "   ", "\t", "\n", "\r\n", "  \t "] {
            assert_eq!(
                pattern_problem(command),
                Some(PolicyReason::EmptyBashPattern),
                "{command:?} should fail closed as empty"
            );
        }
    }

    #[test]
    fn pattern_nul_and_untokenizable_fail_closed() {
        for command in [
            "\0",
            "echo\0",
            "git push\0",
            "echo 'unclosed",
            "echo \"unclosed",
            "'",
            "echo a\\",
            "\"a\\",
            "sh -c \"echo '\"",
            "sh -c '\\''",
        ] {
            assert_eq!(
                pattern_problem(command),
                Some(PolicyReason::UnparsableBashPattern),
                "{command:?} should fail closed as unparsable"
            );
        }
    }

    #[test]
    fn raw_shell_metacharacters_fail_closed() {
        for command in [
            "git status && git commit -m x",
            "sleep 1 & git push",
            "echo `git push`",
            "bash -c 'echo `git push`'",
            "echo {a,b}",
            "{ git push; }",
            "echo $(git push)",
            "sh -c 'echo $(git push)'",
            "cat < file",
            "git status\ngit push",
            "git status\rgit push",
            "git log || git push",
            "git push > /dev/null",
            "echo hi > out.txt",
            "cat x | git push",
            "echo 'a;b'",
            "git status; git push",
            "echo a; echo b",
            "(git push)",
            "echo $HOME",
            "echo ${VAR}",
            "echo \\$HOME",
            "echo \"\\$HOME\"",
            "git commit -m 'fix; thing'",
            "echo \"a|b\"",
        ] {
            assert_eq!(
                pattern_problem(command),
                Some(PolicyReason::UnprovableShellSyntax),
                "{command:?} should fail closed as shell syntax"
            );
        }
    }

    #[test]
    fn metacharacters_inside_quotes_are_still_detected() {
        for command in [
            "git commit -m 'fix; thing'",
            "echo 'a;b'",
            "echo \"a|b\"",
            "echo 'a&b'",
            "echo 'a>b'",
        ] {
            assert_eq!(
                pattern_problem(command),
                Some(PolicyReason::UnprovableShellSyntax),
                "{command:?} should fail closed even when quoted"
            );
        }
    }

    #[test]
    fn general_globs_fail_closed() {
        for command in [
            "echo *",
            "echo a?",
            "echo [abc]",
            "echo 'x*y'",
            "echo '?'",
            "echo '['",
            "echo \\*",
            "sh -c 'ls *'",
            "eval 'echo *'",
        ] {
            assert_eq!(
                pattern_problem(command),
                Some(PolicyReason::UnprovableGlobCommand),
                "{command:?} should fail closed as a glob"
            );
        }
    }

    #[test]
    fn shell_executables_are_inspected_or_rejected() {
        for command in [
            "bash script.sh",
            "sh --version",
            "bash",
            "fish",
            "bash -lc 'git push'",
        ] {
            assert_eq!(
                pattern_problem(command),
                Some(PolicyReason::UnsafeShellInvocation),
                "{command:?} should fail closed as an unsafe shell invocation"
            );
        }

        for command in ["sh -c", "bash -c", "fish -c", "zsh -c", "/usr/bin/bash -c"] {
            assert_eq!(
                pattern_problem(command),
                Some(PolicyReason::ShellCommandMissing),
                "{command:?} should fail closed as a missing shell command"
            );
        }

        for command in [
            "bash -c 'git push'",
            "sh -c 'git add .'",
            "zsh -c 'git push'",
            "dash -c 'git push'",
            "/bin/bash -c 'git push'",
            "bash -e -c 'git push'",
            "bash -c 'bash -c \"git push\"'",
            "sh -c 'env git push'",
            "sh -c 'eval git push'",
            "env bash -c 'git push'",
            "sudo bash -c 'git push'",
        ] {
            assert_eq!(
                pattern_problem(command),
                Some(PolicyReason::GitWriteBlocked),
                "{command:?} should expose the nested git write"
            );
        }

        for command in [
            "sh -c 'git status'",
            "sh -c 'echo hi'",
            "sh -c ''",
            "sh -c ' '",
            "bash -c 'git status'",
        ] {
            assert_eq!(
                pattern_problem(command),
                None,
                "{command:?} should be allowed"
            );
        }
    }

    #[test]
    fn eval_rechecks_joined_arguments() {
        for command in ["eval git push", "eval \"git push\"", "eval echo 'git push'"] {
            assert_eq!(
                pattern_problem(command),
                Some(PolicyReason::GitWriteBlocked),
                "{command:?} should expose the joined git write"
            );
        }

        assert_eq!(
            pattern_problem("eval 'echo $HOME'"),
            Some(PolicyReason::UnprovableShellSyntax)
        );
        assert_eq!(
            pattern_problem("eval 'echo a; echo b'"),
            Some(PolicyReason::UnprovableShellSyntax)
        );
        assert_eq!(
            pattern_problem("eval 'echo *'"),
            Some(PolicyReason::UnprovableGlobCommand)
        );
        assert_eq!(
            pattern_problem("eval"),
            Some(PolicyReason::EmptyBashPattern)
        );

        for command in [
            "eval 'git status'",
            "eval echo hi",
            "eval 'echo hi'",
            "eval \"echo 'git push'\"",
        ] {
            assert_eq!(
                pattern_problem(command),
                None,
                "{command:?} should be allowed"
            );
        }
    }

    #[test]
    fn bash_pattern_allows_safe_simple_commands() {
        for command in [
            "ls -la",
            "true",
            "cat module.py",
            "pytest -q",
            "printf '%s\\n' 'a b'",
            "echo 'a=b'",
            "echo 'hello world'",
            "grep -n 'foo bar' module.py",
            "echo a\\ b",
            "echo a\tb",
            "FOO=bar",
            "FOO=bar .venv/bin/pytest -q",
            "A=1 B=two .venv/bin/pytest -q",
            "FOO=* cmd",
            "git status",
            "git log --oneline -5",
            "git diff --check",
            "env -i git status",
            "command ls -la",
            "sudo ls -la",
        ] {
            assert_eq!(
                pattern_problem(command),
                None,
                "{command:?} should be allowed"
            );
        }
    }

    #[test]
    fn bash_pattern_decision_matches_problem() {
        assert_eq!(
            bash_pattern_decision("git push").reason(),
            Some(PolicyReason::GitWriteBlocked)
        );
        assert_eq!(
            bash_pattern_decision("echo $HOME").reason(),
            Some(PolicyReason::UnprovableShellSyntax)
        );
        assert_eq!(bash_pattern_decision("git status").reason(), None);
        assert!(bash_pattern_decision("git status").is_allow());
        assert!(!bash_pattern_decision("echo $HOME").is_allow());
    }

    #[test]
    fn new_policy_reason_strings_match_reference_categories() {
        for (reason, expected) in [
            (PolicyReason::EmptyBashPattern, "empty_bash_pattern"),
            (
                PolicyReason::UnprovableShellSyntax,
                "unprovable_shell_syntax",
            ),
            (
                PolicyReason::UnprovableGlobCommand,
                "unprovable_glob_command",
            ),
            (
                PolicyReason::UnsafeShellInvocation,
                "unsafe_shell_invocation",
            ),
            (PolicyReason::ShellCommandMissing, "shell_command_missing"),
            (
                PolicyReason::UnparsableBashPattern,
                "unparsable_bash_pattern",
            ),
        ] {
            assert_eq!(reason.as_str(), expected);
            assert_eq!(reason.to_string(), expected);
        }
    }

    #[test]
    fn pattern_decisions_and_reasons_do_not_reveal_input() {
        let command = "echo super-secret; cat /etc/passwd";
        let rendered = format!(
            "{:?} {:?} {}",
            bash_pattern_decision(command),
            pattern_problem(command).expect("shell syntax should be blocked"),
            pattern_problem(command).expect("shell syntax should be blocked")
        );
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("super"));
        assert!(!rendered.contains("passwd"));
        assert!(!rendered.contains("echo"));
    }

    /// Task 4.3 subset of `docs/fixtures/command-policy-cases.json`: raw shell
    /// syntax, globs, shell executables and `eval`. Cases whose behavior is
    /// token-level 4.2 or verifier-envelope (`missing_executable`) are covered
    /// elsewhere or deferred to task 5.1.
    #[test]
    fn reference_corpus_cases_4_3() {
        let allow = [
            ("FOO=bar", None),
            ("true", None),
            ("cat module.py", None),
            ("printf '%s\\n' 'a b'", None),
            ("echo 'a=b'", None),
            ("echo 'hello world'", None),
            ("grep -n 'foo bar' module.py", None),
        ];
        for (command, reason) in allow {
            assert_eq!(
                pattern_problem(command),
                reason,
                "{command:?} should be allowed"
            );
        }

        let deny = [
            ("git status && git commit -m x", "unprovable_shell_syntax"),
            ("sleep 1 & git push", "unprovable_shell_syntax"),
            ("echo `git push`", "unprovable_shell_syntax"),
            ("bash -c 'echo `git push`'", "unprovable_shell_syntax"),
            ("echo {a,b}", "unprovable_shell_syntax"),
            ("{ git push; }", "unprovable_shell_syntax"),
            ("echo $(git push)", "unprovable_shell_syntax"),
            ("sh -c 'echo $(git push)'", "unprovable_shell_syntax"),
            ("cat < file", "unprovable_shell_syntax"),
            ("git status\ngit push", "unprovable_shell_syntax"),
            ("git log || git push", "unprovable_shell_syntax"),
            ("git push > /dev/null", "unprovable_shell_syntax"),
            ("echo hi > out.txt", "unprovable_shell_syntax"),
            ("cat x | git push", "unprovable_shell_syntax"),
            ("git commit -m 'fix; thing'", "unprovable_shell_syntax"),
            ("echo 'x*y'", "unprovable_glob_command"),
            ("echo 'a;b'", "unprovable_shell_syntax"),
            ("git status; git push", "unprovable_shell_syntax"),
            ("echo a; echo b", "unprovable_shell_syntax"),
            ("bash -c 'git push'", "git_write_blocked"),
            ("sh -c 'git add .'", "git_write_blocked"),
            ("sh -c 'git push'", "git_write_blocked"),
            ("zsh -c 'git push'", "git_write_blocked"),
            ("dash -c 'git push'", "git_write_blocked"),
            ("sh -c", "shell_command_missing"),
            ("bash script.sh", "unsafe_shell_invocation"),
            ("(git push)", "unprovable_shell_syntax"),
            ("echo $HOME", "unprovable_shell_syntax"),
            ("echo ${VAR}", "unprovable_shell_syntax"),
            ("eval git push", "git_write_blocked"),
            ("eval \"git push\"", "git_write_blocked"),
            ("", "empty_bash_pattern"),
            ("   ", "empty_bash_pattern"),
        ];
        for (command, category) in deny {
            let reason = pattern_problem(command).expect("case should be denied");
            assert_eq!(
                reason.as_str(),
                category,
                "{command:?} should be denied with {category}"
            );
        }
    }

    /// Representative subset of `docs/fixtures/command-policy-cases.json` whose
    /// behavior falls inside the task 4.2 token-level scope. Cases that need raw
    /// shell metacharacter scanning, general globs, shell executables or `eval`
    /// are covered by the task 4.3 tests above.
    #[test]
    fn reference_corpus_cases_in_scope() {
        let allow = [
            "env -i git status",
            "env FOO=bar git log",
            "env -u FOO git status",
            "env -C /repo git status",
            "env -- git status",
            "git -C /repo status --short",
            "git log --oneline -5",
            "git diff --check",
            "FOO=bar .venv/bin/pytest -q",
            "A=1 B=two .venv/bin/pytest -q",
            ".venv/bin/pytest -q",
            "command ls -la",
            "sudo ls -la",
        ];
        for command in allow {
            assert_eq!(problem(command), None, "{command:?} should be allowed");
        }

        let deny = [
            ("git add .", PolicyReason::GitWriteBlocked),
            ("git commit -m x", PolicyReason::GitWriteBlocked),
            ("git push origin main", PolicyReason::GitWriteBlocked),
            ("git -C /repo commit -m x", PolicyReason::GitWriteBlocked),
            ("git -c user.name=x push", PolicyReason::GitWriteBlocked),
            (
                "git --git-dir=/repo/.git push",
                PolicyReason::GitWriteBlocked,
            ),
            (
                "git --git-dir /repo/.git push",
                PolicyReason::GitWriteBlocked,
            ),
            ("git --work-tree=/repo push", PolicyReason::GitWriteBlocked),
            ("git --namespace=x push", PolicyReason::GitWriteBlocked),
            (
                "git -C /repo -c x=y commit -m z",
                PolicyReason::GitWriteBlocked,
            ),
            ("git    add .", PolicyReason::GitWriteBlocked),
            ("git *", PolicyReason::UnprovableGitGlob),
            ("git ad*", PolicyReason::UnprovableGitGlob),
            ("/usr/bin/git add .", PolicyReason::GitWriteBlocked),
            ("./git commit -m x", PolicyReason::GitWriteBlocked),
            ("FOO=bar git push", PolicyReason::GitWriteBlocked),
            ("env git add .", PolicyReason::GitWriteBlocked),
            ("sudo git push", PolicyReason::GitWriteBlocked),
            ("command git commit -m x", PolicyReason::GitWriteBlocked),
            ("nohup git push", PolicyReason::GitWriteBlocked),
            ("nice git push", PolicyReason::GitWriteBlocked),
            ("time git push", PolicyReason::GitWriteBlocked),
            ("exec git push", PolicyReason::GitWriteBlocked),
            ("env -S 'git push'", PolicyReason::UnprovableWrapperCommand),
            (
                "env --split-string 'git add .'",
                PolicyReason::UnprovableWrapperCommand,
            ),
            ("env -Sgit push", PolicyReason::UnprovableWrapperCommand),
            (
                "env -i -S 'git commit -m x'",
                PolicyReason::UnprovableWrapperCommand,
            ),
            (
                "env FOO=bar -S 'git push'",
                PolicyReason::UnprovableWrapperCommand,
            ),
            (
                "env -S 'git status'",
                PolicyReason::UnprovableWrapperCommand,
            ),
        ];
        for (command, reason) in deny {
            assert_eq!(
                problem(command),
                Some(reason),
                "{command:?} should be denied"
            );
        }
    }

    /// Returns the single verifier-level problem reason for one command.
    fn test_reason(command: &str) -> Option<TestCommandReason> {
        validate_test_commands(&[command])
            .into_iter()
            .next()
            .map(TestCommandProblem::reason)
    }

    #[test]
    fn validate_test_commands_empty_list_is_valid() {
        assert!(validate_test_commands(&[]).is_empty());
    }

    #[test]
    fn validate_test_commands_accept_safe_test_command_cases() {
        for command in [
            "env -i git status",
            "env FOO=bar git log",
            "env -u FOO git status",
            "env -C /repo git status",
            "env -- git status",
            "git -C /repo status --short",
            "FOO=bar .venv/bin/pytest -q",
            "A=1 B=two .venv/bin/pytest -q",
            ".venv/bin/pytest -q",
            "printf '%s\\n' 'a b'",
            "echo 'a=b'",
            "echo 'hello world'",
            "grep -n 'foo bar' module.py",
            "git status",
            "git log --oneline -5",
            "git diff --check",
        ] {
            assert!(
                validate_test_commands(&[command]).is_empty(),
                "{command:?} should be allowed"
            );
            assert_eq!(test_reason(command), None, "{command:?} should be allowed");
        }
    }

    #[test]
    fn validate_test_commands_deny_all_test_command_fixture_cases() {
        let cases = [
            ("git status && git commit -m x", "unprovable_shell_syntax"),
            ("sleep 1 & git push", "unprovable_shell_syntax"),
            ("echo `git push`", "unprovable_shell_syntax"),
            ("bash -c 'echo `git push`'", "unprovable_shell_syntax"),
            ("echo {a,b}", "unprovable_shell_syntax"),
            ("{ git push; }", "unprovable_shell_syntax"),
            ("echo $(git push)", "unprovable_shell_syntax"),
            ("sh -c 'echo $(git push)'", "unprovable_shell_syntax"),
            ("", "empty_bash_pattern"),
            ("   ", "empty_bash_pattern"),
            ("env -S 'git push'", "unprovable_wrapper_command"),
            (
                "env --split-string 'git add .'",
                "unprovable_wrapper_command",
            ),
            ("env -Sgit push", "unprovable_wrapper_command"),
            ("env -i -S 'git commit -m x'", "unprovable_wrapper_command"),
            ("env FOO=bar -S 'git push'", "unprovable_wrapper_command"),
            ("env -S 'git status'", "unprovable_wrapper_command"),
            ("eval git push", "git_write_blocked"),
            ("eval \"git push\"", "git_write_blocked"),
            ("git commit -m x", "git_write_blocked"),
            ("git *", "unprovable_git_glob"),
            ("git ad*", "unprovable_git_glob"),
            ("git push origin main", "git_write_blocked"),
            ("/usr/bin/git add .", "git_write_blocked"),
            ("./git commit -m x", "git_write_blocked"),
            ("git -C /repo commit -m x", "git_write_blocked"),
            ("git -c user.name=x push", "git_write_blocked"),
            ("git --git-dir=/repo/.git push", "git_write_blocked"),
            ("git --git-dir /repo/.git push", "git_write_blocked"),
            ("git --work-tree=/repo push", "git_write_blocked"),
            ("git --namespace=x push", "git_write_blocked"),
            ("git -C /repo -c x=y commit -m z", "git_write_blocked"),
            ("git    add .", "git_write_blocked"),
            ("git commit -m 'fix thing'", "git_write_blocked"),
            ("cat < file", "unprovable_shell_syntax"),
            ("git status\ngit push", "unprovable_shell_syntax"),
            ("git log || git push", "unprovable_shell_syntax"),
            ("git push > /dev/null", "unprovable_shell_syntax"),
            ("echo hi > out.txt", "unprovable_shell_syntax"),
            ("cat x | git push", "unprovable_shell_syntax"),
            ("git commit -m 'fix; thing'", "unprovable_shell_syntax"),
            ("echo 'x*y'", "unprovable_glob_command"),
            ("echo 'a;b'", "unprovable_shell_syntax"),
            ("git status; git push", "unprovable_shell_syntax"),
            ("echo a; echo b", "unprovable_shell_syntax"),
            ("sh -c", "shell_command_missing"),
            ("bash script.sh", "unsafe_shell_invocation"),
            ("(git push)", "unprovable_shell_syntax"),
            ("echo $HOME", "unprovable_shell_syntax"),
            ("echo ${VAR}", "unprovable_shell_syntax"),
            ("env git add .", "git_write_blocked"),
        ];
        for (command, category) in cases {
            let problems = validate_test_commands(&[command]);
            assert_eq!(problems.len(), 1, "{command:?} should be rejected");
            assert_eq!(problems[0].index(), 0, "{command:?} index should be 0");
            assert_eq!(
                problems[0].reason().as_str(),
                category,
                "{command:?} should be denied with {category}"
            );
        }
    }

    #[test]
    fn validate_test_commands_deny_assignment_only_as_missing_executable() {
        for command in ["FOO=bar", "FOO=bar BAZ=qux"] {
            let problems = validate_test_commands(&[command]);
            assert_eq!(problems.len(), 1, "{command:?} should be rejected");
            assert_eq!(problems[0].index(), 0);
            assert_eq!(
                problems[0].reason(),
                TestCommandReason::MissingExecutable,
                "{command:?} should be missing_executable"
            );
            assert_eq!(problems[0].reason().as_str(), "missing_executable");
        }
    }

    #[test]
    fn validate_test_commands_preserve_order_and_indices() {
        let problems = validate_test_commands(&["git status", "FOO=bar", "git push", ""]);
        let seen: Vec<(usize, &str)> = problems
            .iter()
            .map(|problem| (problem.index(), problem.reason().as_str()))
            .collect();
        assert_eq!(
            seen,
            vec![
                (1, "missing_executable"),
                (2, "git_write_blocked"),
                (3, "empty_bash_pattern"),
            ]
        );
    }

    #[test]
    fn test_command_reason_strings_match_reference_categories() {
        assert_eq!(
            TestCommandReason::MissingExecutable.as_str(),
            "missing_executable"
        );
        assert_eq!(
            TestCommandReason::Policy(PolicyReason::GitWriteBlocked).as_str(),
            "git_write_blocked"
        );
        assert_eq!(
            TestCommandReason::MissingExecutable.to_string(),
            "missing_executable"
        );
    }

    #[test]
    fn test_command_problems_do_not_reveal_input() {
        let problems = validate_test_commands(&[
            "git push super-secret-token",
            "TOKEN=super-secret-value",
            "echo secret; cat /etc/passwd",
        ]);
        assert_eq!(problems.len(), 3);
        let rendered = format!("{problems:?}");
        for needle in ["secret", "super", "push", "TOKEN", "passwd", "echo", "git"] {
            assert!(
                !rendered.contains(needle),
                "Debug output leaked {needle:?}: {rendered}"
            );
        }
        for problem in &problems {
            let display = problem.reason().to_string();
            for needle in ["secret", "super", "push", "TOKEN", "passwd"] {
                assert!(
                    !display.contains(needle),
                    "Display output leaked {needle:?}: {display}"
                );
            }
        }
    }
}
