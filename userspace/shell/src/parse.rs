//! Command-line parser and pipeline planner.
//!
//! This module is **pure**: no `unsafe`, no syscalls, no allocation, no
//! target coupling. Every `&str` it produces borrows from the caller's input
//! line, so a `Pipeline` cannot outlive the line it was parsed from. That is
//! what makes the whole thing testable with a plain `cargo test` on the
//! host, following the same "pure decision core, unsafe glue elsewhere"
//! discipline as `decide_next_signal` and `reader_step_when_empty` in the
//! kernel.
//!
//! ## Grammar (deliberately tiny)
//!
//! ```text
//!   line     := stage ('|' stage)*
//!   stage    := token+ (redirect | token)*
//!   redirect := ('>' | '>>' | '<') token
//! ```
//!
//! `|`, `>`, `>>` and `<` are recognised as tokens in their own right even
//! with no surrounding whitespace, so `echo hi>out` splits into
//! `echo`, `hi`, `>`, `out`. This is why the tokenizer scans bytes rather
//! than using `split_whitespace`, which would glue `hi>out` together.
//!
//! ## Out of scope (documented, not accidental)
//!
//! No quoting (`"a b"` is two tokens), no escaping (`\ `), no variable
//! expansion (`$X` is literal), no globbing (`*.txt` is literal), no
//! background (`&`), no `;`/`&&`/`||` sequencing, no here-docs, no fd
//! dup syntax (`2>&1`). Commands are absolute paths — there is no PATH
//! search (see `exec`).

pub const MAX_ARGS: usize = 16;
pub const MAX_PIPELINE_STAGES: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RedirectKind {
    None,
    /// `>` — truncate (or create).
    Out,
    /// `>>` — append.
    OutAppend,
    /// `<` — read.
    In,
}

/// One command in a pipeline, with its own redirections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stage<'a> {
    pub argv: [&'a str; MAX_ARGS],
    pub argc: usize,
    /// Target of `>` / `>>`, if the stage had one.
    pub redirect_out: Option<(&'a str, RedirectKind)>,
    /// Source of `<`, if the stage had one.
    pub redirect_in: Option<&'a str>,
}

impl<'a> Stage<'a> {
    const fn empty() -> Self {
        Self {
            argv: [""; MAX_ARGS],
            argc: 0,
            redirect_out: None,
            redirect_in: None,
        }
    }
    /// The stage's arguments, `argv[0]` (the program path) first.
    pub fn args(&self) -> &[&'a str] {
        &self.argv[..self.argc]
    }
}

/// A parsed command line: one or more stages joined by `|`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pipeline<'a> {
    pub stages: [Stage<'a>; MAX_PIPELINE_STAGES],
    pub stage_count: usize,
}

impl<'a> Pipeline<'a> {
    /// The stages that were actually parsed.
    pub fn stages(&self) -> &[Stage<'a>] {
        &self.stages[..self.stage_count]
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ParseError {
    /// Blank or whitespace-only input. The REPL treats this as "reprompt",
    /// not as an error worth printing.
    Empty,
    TooManyStages,
    TooManyArgs,
    /// e.g. `a | | b` — a pipe with nothing between it and the next.
    EmptyStage,
    /// e.g. `a >` — a redirection operator with no filename after it.
    MissingRedirectTarget,
    /// e.g. `a |` — the line ends on a pipe.
    TrailingPipe,
}

impl ParseError {
    /// A short, fixed message for the REPL to print. No formatting.
    pub fn message(&self) -> &'static str {
        match self {
            ParseError::Empty => "empty",
            ParseError::TooManyStages => "too many pipeline stages",
            ParseError::TooManyArgs => "too many arguments",
            ParseError::EmptyStage => "syntax error: empty command between pipes",
            ParseError::MissingRedirectTarget => "syntax error: missing redirection target",
            ParseError::TrailingPipe => "syntax error: pipeline ends with '|'",
        }
    }
}

/// A single lexical token.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Tok<'a> {
    Word(&'a str),
    Pipe,
    Great,
    GreatGreat,
    Less,
}

#[inline]
fn is_space(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\r'
}

#[inline]
fn is_op(b: u8) -> bool {
    b == b'|' || b == b'>' || b == b'<'
}

/// Byte-wise tokenizer. Returns the next token and the offset just past it,
/// or `None` once only whitespace remains.
fn next_token(line: &str, mut i: usize) -> Option<(Tok<'_>, usize)> {
    let b = line.as_bytes();
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    if i >= b.len() {
        return None;
    }
    match b[i] {
        b'|' => Some((Tok::Pipe, i + 1)),
        b'<' => Some((Tok::Less, i + 1)),
        b'>' => {
            // `>>` must be recognised before `>`, or append silently
            // degrades into truncate-then-truncate.
            if i + 1 < b.len() && b[i + 1] == b'>' {
                Some((Tok::GreatGreat, i + 2))
            } else {
                Some((Tok::Great, i + 1))
            }
        }
        _ => {
            // A word runs until whitespace or the next operator byte, so
            // `hi>out` yields `hi` here and `>` on the following call.
            let start = i;
            while i < b.len() && !is_space(b[i]) && !is_op(b[i]) {
                i += 1;
            }
            Some((Tok::Word(&line[start..i]), i))
        }
    }
}

/// Parse one line into a [`Pipeline`].
///
/// All returned `&str`s borrow from `line`, so the pipeline is tied to its
/// lifetime — no allocation happens anywhere.
pub fn parse_line(line: &str) -> Result<Pipeline<'_>, ParseError> {
    let mut pipeline = Pipeline {
        stages: [Stage::empty(); MAX_PIPELINE_STAGES],
        stage_count: 0,
    };

    let mut cur = Stage::empty();
    let mut cur_has_content = false;
    let mut any_token = false;
    let mut i = 0usize;

    while let Some((tok, next)) = next_token(line, i) {
        i = next;
        any_token = true;
        match tok {
            Tok::Word(w) => {
                if cur.argc == MAX_ARGS {
                    return Err(ParseError::TooManyArgs);
                }
                cur.argv[cur.argc] = w;
                cur.argc += 1;
                cur_has_content = true;
            }
            Tok::Pipe => {
                // `| ...` with nothing before it, or `a | | b`.
                if !cur_has_content {
                    return Err(ParseError::EmptyStage);
                }
                if pipeline.stage_count == MAX_PIPELINE_STAGES {
                    return Err(ParseError::TooManyStages);
                }
                pipeline.stages[pipeline.stage_count] = cur;
                pipeline.stage_count += 1;
                cur = Stage::empty();
                cur_has_content = false;
            }
            Tok::Great | Tok::GreatGreat | Tok::Less => {
                let kind = match tok {
                    Tok::Great => RedirectKind::Out,
                    Tok::GreatGreat => RedirectKind::OutAppend,
                    _ => RedirectKind::In,
                };
                // The target must be a plain word: `a > | b` and `a >` are
                // both malformed.
                let (target, after) = match next_token(line, i) {
                    Some((Tok::Word(w), after)) => (w, after),
                    _ => return Err(ParseError::MissingRedirectTarget),
                };
                i = after;
                if kind == RedirectKind::In {
                    cur.redirect_in = Some(target);
                } else {
                    cur.redirect_out = Some((target, kind));
                }
                // A redirection alone does not make a command ("> out" is
                // still an empty stage), so `cur_has_content` is untouched.
            }
        }
    }

    if !any_token {
        return Err(ParseError::Empty);
    }
    if !cur_has_content {
        // The line ended right after a `|`, or the final stage was only a
        // redirection with no command.
        return Err(if pipeline.stage_count > 0 {
            ParseError::TrailingPipe
        } else {
            ParseError::EmptyStage
        });
    }
    if pipeline.stage_count == MAX_PIPELINE_STAGES {
        return Err(ParseError::TooManyStages);
    }
    pipeline.stages[pipeline.stage_count] = cur;
    pipeline.stage_count += 1;

    Ok(pipeline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_command_with_arg() {
        let p = parse_line("echo hi").unwrap();
        assert_eq!(p.stage_count, 1);
        assert_eq!(p.stages[0].argc, 2);
        assert_eq!(p.stages[0].args(), &["echo", "hi"]);
        assert_eq!(p.stages[0].redirect_out, None);
        assert_eq!(p.stages[0].redirect_in, None);
    }

    #[test]
    fn empty_line_is_empty_error() {
        assert_eq!(parse_line(""), Err(ParseError::Empty));
    }

    #[test]
    fn whitespace_only_is_empty_error() {
        assert_eq!(parse_line("   "), Err(ParseError::Empty));
        assert_eq!(parse_line(" \t \r "), Err(ParseError::Empty));
    }

    #[test]
    fn three_stage_pipeline() {
        let p = parse_line("a | b | c").unwrap();
        assert_eq!(p.stage_count, 3);
        assert_eq!(p.stages[0].args(), &["a"]);
        assert_eq!(p.stages[1].args(), &["b"]);
        assert_eq!(p.stages[2].args(), &["c"]);
    }

    #[test]
    fn trailing_pipe_is_error() {
        assert_eq!(parse_line("a |"), Err(ParseError::TrailingPipe));
        assert_eq!(parse_line("a |   "), Err(ParseError::TrailingPipe));
    }

    #[test]
    fn empty_stage_between_pipes_is_error() {
        assert_eq!(parse_line("a | | b"), Err(ParseError::EmptyStage));
    }

    #[test]
    fn leading_pipe_is_empty_stage() {
        assert_eq!(parse_line("| b"), Err(ParseError::EmptyStage));
    }

    #[test]
    fn redirect_out_with_spaces() {
        let p = parse_line("a > out.txt").unwrap();
        assert_eq!(p.stages[0].redirect_out, Some(("out.txt", RedirectKind::Out)));
        // The target must not leak into argv.
        assert_eq!(p.stages[0].args(), &["a"]);
    }

    #[test]
    fn redirect_out_without_spaces_tokenizes_the_same() {
        // Proves the tokenizer scans bytes instead of relying on
        // whitespace-adjacency: "a>out.txt" must parse identically.
        let spaced = parse_line("a > out.txt").unwrap();
        let tight = parse_line("a>out.txt").unwrap();
        assert_eq!(tight.stages[0].redirect_out, Some(("out.txt", RedirectKind::Out)));
        assert_eq!(tight.stages[0].args(), &["a"]);
        assert_eq!(spaced, tight);
    }

    #[test]
    fn append_redirect_is_distinct_from_truncate() {
        let p = parse_line("a >> out.txt").unwrap();
        assert_eq!(
            p.stages[0].redirect_out,
            Some(("out.txt", RedirectKind::OutAppend))
        );
        // ">>" must not be mistaken for two ">" tokens.
        let tight = parse_line("a>>out.txt").unwrap();
        assert_eq!(
            tight.stages[0].redirect_out,
            Some(("out.txt", RedirectKind::OutAppend))
        );
    }

    #[test]
    fn redirect_in() {
        let p = parse_line("a < in.txt").unwrap();
        assert_eq!(p.stages[0].redirect_in, Some("in.txt"));
        assert_eq!(p.stages[0].args(), &["a"]);
        let tight = parse_line("a<in.txt").unwrap();
        assert_eq!(tight.stages[0].redirect_in, Some("in.txt"));
    }

    #[test]
    fn redirect_without_target_is_error() {
        assert_eq!(parse_line("a >"), Err(ParseError::MissingRedirectTarget));
        assert_eq!(parse_line("a <"), Err(ParseError::MissingRedirectTarget));
        assert_eq!(parse_line("a >>"), Err(ParseError::MissingRedirectTarget));
        // An operator where a filename belongs is equally malformed.
        assert_eq!(parse_line("a > | b"), Err(ParseError::MissingRedirectTarget));
    }

    #[test]
    fn too_many_stages_is_error() {
        // MAX_PIPELINE_STAGES stages is fine...
        let ok = "a | a | a | a | a | a | a | a"; // 8
        assert_eq!(parse_line(ok).unwrap().stage_count, MAX_PIPELINE_STAGES);
        // ...one more is not.
        let too_many = "a | a | a | a | a | a | a | a | a"; // 9
        assert_eq!(parse_line(too_many), Err(ParseError::TooManyStages));
    }

    #[test]
    fn too_many_args_is_error() {
        // MAX_ARGS words in one stage is fine.
        let ok = "a b c d e f g h i j k l m n o p"; // 16
        assert_eq!(parse_line(ok).unwrap().stages[0].argc, MAX_ARGS);
        // The 17th overflows.
        let too_many = "a b c d e f g h i j k l m n o p q"; // 17
        assert_eq!(parse_line(too_many), Err(ParseError::TooManyArgs));
    }

    #[test]
    fn middle_stage_can_have_both_pipe_input_and_explicit_redirect_in() {
        // The parser's job is only to represent "this stage has an explicit
        // input redirect" unambiguously; the executor decides that `<` wins
        // over the incoming pipe (that precedence is not host-testable).
        let p = parse_line("a | b < in.txt | c").unwrap();
        assert_eq!(p.stage_count, 3);
        assert_eq!(p.stages[1].args(), &["b"]);
        assert_eq!(p.stages[1].redirect_in, Some("in.txt"));
        assert_eq!(p.stages[0].redirect_in, None);
        assert_eq!(p.stages[2].redirect_in, None);
    }

    #[test]
    fn round_trip_against_hand_built_pipeline() {
        let parsed = parse_line("  /bin/echo hi   |  /bin/cat > /tmp/o.txt ").unwrap();

        let mut s0 = Stage::empty();
        s0.argv[0] = "/bin/echo";
        s0.argv[1] = "hi";
        s0.argc = 2;

        let mut s1 = Stage::empty();
        s1.argv[0] = "/bin/cat";
        s1.argc = 1;
        s1.redirect_out = Some(("/tmp/o.txt", RedirectKind::Out));

        let mut expected = Pipeline {
            stages: [Stage::empty(); MAX_PIPELINE_STAGES],
            stage_count: 2,
        };
        expected.stages[0] = s0;
        expected.stages[1] = s1;

        assert_eq!(parsed, expected);
        assert_eq!(parsed.stages(), expected.stages());
    }

    #[test]
    fn redirects_can_precede_and_follow_args() {
        // "< in a b > out" — redirections may appear anywhere in the stage.
        let p = parse_line("< in.txt a b > out.txt").unwrap();
        assert_eq!(p.stages[0].args(), &["a", "b"]);
        assert_eq!(p.stages[0].redirect_in, Some("in.txt"));
        assert_eq!(p.stages[0].redirect_out, Some(("out.txt", RedirectKind::Out)));
    }

    #[test]
    fn redirection_only_stage_is_empty_stage_error() {
        // "> out" has a target but no command to run.
        assert_eq!(parse_line("> out.txt"), Err(ParseError::EmptyStage));
    }

    #[test]
    fn extra_whitespace_is_ignored() {
        let p = parse_line("   a    b  |   c   ").unwrap();
        assert_eq!(p.stage_count, 2);
        assert_eq!(p.stages[0].args(), &["a", "b"]);
        assert_eq!(p.stages[1].args(), &["c"]);
    }

    #[test]
    fn last_redirect_of_a_kind_wins() {
        // Not a shell-compat guarantee, just pinning the documented
        // behaviour: a later target overwrites an earlier one.
        let p = parse_line("a > one.txt > two.txt").unwrap();
        assert_eq!(p.stages[0].redirect_out, Some(("two.txt", RedirectKind::Out)));
    }

    #[test]
    fn parse_error_messages_are_non_empty() {
        for e in [
            ParseError::Empty,
            ParseError::TooManyStages,
            ParseError::TooManyArgs,
            ParseError::EmptyStage,
            ParseError::MissingRedirectTarget,
            ParseError::TrailingPipe,
        ] {
            assert!(!e.message().is_empty());
        }
    }
}
