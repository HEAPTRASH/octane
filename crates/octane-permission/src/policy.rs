//! Rule sets and evaluation.

use serde::{Deserialize, Serialize};

use crate::matcher::RuleMatcher;
use crate::resource::{Action, Resource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Auto-approved.
    Allow,
    /// Pause and ask the user.
    Ask,
    /// Refused outright. Never presented to the user, because a prompt the user
    /// can say yes to is not a denial.
    Deny,
}

/// Where a rule came from. Narrower scopes are more specific, but note that
/// scope does **not** override severity — see [`Policy::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Machine- or org-managed. Cannot be relaxed by lower scopes.
    Managed,
    /// `~/.octane/settings.json`.
    User,
    /// `<repo>/.octane/settings.json`.
    Project,
    /// Granted interactively during this session.
    Session,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub action: Action,
    pub decision: Decision,
    pub scope: Scope,
    matcher: RuleMatcher,
    /// Original text, for explaining a decision back to the user.
    source: String,
}

impl Rule {
    pub fn new(resource_pattern: &str, decision: Decision, scope: Scope) -> Result<Self, crate::PermissionError> {
        let parsed: Resource = resource_pattern.parse()?;
        Ok(Self {
            action: parsed.action,
            decision,
            scope,
            matcher: RuleMatcher::compile(parsed.action, &parsed.target),
            source: resource_pattern.to_string(),
        })
    }

    pub fn matches(&self, resource: &Resource) -> bool {
        self.action == resource.action && self.matcher.matches(resource)
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

/// An evaluated decision, with the reason attached.
///
/// The reason is not decoration. "Why did it ask me that?" is the most common
/// question a permission system has to answer, and a system that cannot answer
/// it gets disabled wholesale.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub decision: Decision,
    pub reason: Reason,
}

#[derive(Debug, Clone)]
pub enum Reason {
    /// Matched an explicit rule.
    Rule { source: String, scope: Scope },
    /// Inside the workspace, which is allowed for reads and writes by default.
    WorkspaceDefault,
    /// The active mode forced this regardless of rules.
    Mode(crate::mode::PermissionMode),
    /// Nothing matched; the action's fail-closed default applied.
    ActionDefault,
    /// A previously granted session scope already covers this.
    ExistingGrant { source: String },
}

#[derive(Debug, Default)]
pub struct Policy {
    rules: Vec<Rule>,
    /// Paths that are read/write by default without a prompt. Everything else
    /// starts at `Ask`.
    workspace_roots: Vec<String>,
}

impl Policy {
    pub fn builder() -> PolicyBuilder {
        PolicyBuilder::default()
    }

    /// Resolve a resource to a decision.
    ///
    /// **Severity wins over specificity: `Deny > Ask > Allow`.** A broad
    /// `ask(command(*))` therefore beats a narrow `allow(command(git))`.
    ///
    /// This is Antigravity's convention, chosen deliberately over opencode's
    /// last-match-wins. A user who writes a broad `ask` rule means it as a floor,
    /// and surprising someone *upward* into less oversight than they configured
    /// is far worse than surprising them downward into an extra prompt.
    ///
    /// One consequence to keep in mind: to carve an exception out of a broad
    /// `ask`, the user must narrow the `ask` rule itself, not add an `allow`.
    pub fn evaluate(&self, resource: &Resource, mode: crate::mode::PermissionMode) -> Verdict {
        // Deny always wins, and no mode can override it. This is the one rung
        // that must hold even under `bypass`, or "bypass" silently means "root".
        if let Some(rule) = self.first_match(resource, Decision::Deny) {
            return Verdict {
                decision: Decision::Deny,
                reason: Reason::Rule { source: rule.source.clone(), scope: rule.scope },
            };
        }

        // A mode may force `Ask` (plan) or skip the remaining prompts (bypass),
        // but only after deny rules have had their say.
        //
        // Checked before session grants on purpose: a grant made before the user
        // switched into `plan` must not survive the switch.
        if let Some(forced) = mode.force(resource) {
            return Verdict { decision: forced, reason: Reason::Mode(mode) };
        }

        // An interactive grant made *this session* outranks a configured `ask`.
        //
        // This is the one documented exception to severity-over-specificity, and
        // it is load-bearing. Without it, the common configuration
        // `ask(command(*))` makes "remember my answer" impossible: the standing
        // ask rule would re-fire on every identical call, and prompt fatigue is
        // precisely what drives users to disable permissions wholesale.
        //
        // The justification is that the two signals are not the same kind of
        // thing. `ask(command(*))` says *check with me about commands*; answering
        // yes to `command(cargo test)` **is** that check, already performed. Deny
        // rules and modes still win, so nothing the user forbade is reachable.
        if let Some(rule) = self.session_grant(resource) {
            return Verdict {
                decision: Decision::Allow,
                reason: Reason::ExistingGrant { source: rule.source.clone() },
            };
        }

        if let Some(rule) = self.first_match(resource, Decision::Ask) {
            return Verdict {
                decision: Decision::Ask,
                reason: Reason::Rule { source: rule.source.clone(), scope: rule.scope },
            };
        }

        if let Some(rule) = self.first_match(resource, Decision::Allow) {
            return Verdict {
                decision: Decision::Allow,
                reason: Reason::Rule { source: rule.source.clone(), scope: rule.scope },
            };
        }

        // Reading and writing inside the project is the whole point of the tool;
        // prompting for it would make the agent unusable. Note this covers file
        // actions only — `command` never gets a workspace pass.
        if matches!(resource.action, Action::ReadFile | Action::WriteFile)
            && self.in_workspace(&resource.target)
        {
            return Verdict { decision: Decision::Allow, reason: Reason::WorkspaceDefault };
        }

        Verdict { decision: resource.action.default_decision(), reason: Reason::ActionDefault }
    }

    /// An `Allow` recorded interactively during this session.
    fn session_grant(&self, resource: &Resource) -> Option<&Rule> {
        self.rules.iter().find(|rule| {
            rule.scope == Scope::Session && rule.decision == Decision::Allow && rule.matches(resource)
        })
    }

    /// Highest-precedence rule with the given decision, preferring narrower scopes
    /// only to pick *which* matching rule to cite — not to change the outcome.
    fn first_match(&self, resource: &Resource, decision: Decision) -> Option<&Rule> {
        self.rules
            .iter()
            .filter(|rule| rule.decision == decision && rule.matches(resource))
            .max_by_key(|rule| rule.scope)
    }

    fn in_workspace(&self, target: &str) -> bool {
        self.workspace_roots.iter().any(|root| {
            let root = camino::Utf8Path::new(root);
            let target = camino::Utf8Path::new(target);
            target == root || target.starts_with(root)
        })
    }

    /// Record an interactive grant so the same question is not asked twice.
    ///
    /// Prompt fatigue is what drives users to `--dangerously-skip-permissions`,
    /// which turns a calibrated policy into no policy at all.
    pub fn grant_for_session(&mut self, resource: &Resource) -> Result<(), crate::PermissionError> {
        let pattern = resource.to_string();
        self.rules.push(Rule::new(&pattern, Decision::Allow, Scope::Session)?);

        // Write implies read, so an approved write does not then prompt for the
        // read it necessarily performs.
        for implied in resource.implied() {
            self.rules.push(Rule::new(&implied.to_string(), Decision::Allow, Scope::Session)?);
        }
        Ok(())
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

#[derive(Debug, Default)]
pub struct PolicyBuilder {
    rules: Vec<Rule>,
    workspace_roots: Vec<String>,
    errors: Vec<crate::PermissionError>,
}

impl PolicyBuilder {
    pub fn workspace_root(mut self, root: impl Into<String>) -> Self {
        self.workspace_roots.push(root.into());
        self
    }

    pub fn allow(self, pattern: &str, scope: Scope) -> Self {
        self.rule(pattern, Decision::Allow, scope)
    }

    pub fn ask(self, pattern: &str, scope: Scope) -> Self {
        self.rule(pattern, Decision::Ask, scope)
    }

    pub fn deny(self, pattern: &str, scope: Scope) -> Self {
        self.rule(pattern, Decision::Deny, scope)
    }

    fn rule(mut self, pattern: &str, decision: Decision, scope: Scope) -> Self {
        match Rule::new(pattern, decision, scope) {
            Ok(rule) => self.rules.push(rule),
            // Collected rather than panicking: one bad line in a config file
            // should be reported, not fatal.
            Err(error) => self.errors.push(error),
        }
        self
    }

    /// Deny rules every project should have, applied at [`Scope::Managed`] so
    /// nothing downstream can relax them.
    ///
    /// `.git/` is here for the reason described in `RESEARCH.md` §B: writing
    /// `.git/hooks/pre-commit` is arbitrary code execution on the user's next
    /// commit, reached through a tool that only asked to "write a file in the
    /// project". `.octane/` is here so the agent cannot rewrite its own policy.
    pub fn with_baseline_denies(self) -> Self {
        self.deny("write_file(.git/)", Scope::Managed)
            .deny("write_file(.octane/)", Scope::Managed)
            .deny("read_file(.env)", Scope::Managed)
            .deny("command(sudo)", Scope::Managed)
    }

    pub fn build(self) -> (Policy, Vec<crate::PermissionError>) {
        (Policy { rules: self.rules, workspace_roots: self.workspace_roots }, self.errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::PermissionMode;

    fn policy() -> Policy {
        Policy::builder()
            .workspace_root("/proj")
            .allow("command(git)", Scope::User)
            .ask("command(*)", Scope::User)
            .deny("command(rm -rf /)", Scope::Managed)
            .build()
            .0
    }

    #[test]
    fn deny_beats_everything() {
        let verdict = policy().evaluate(&Resource::command("rm -rf /"), PermissionMode::Bypass);
        assert_eq!(verdict.decision, Decision::Deny);
    }

    #[test]
    fn broad_ask_beats_narrow_allow() {
        // The documented consequence of severity-over-specificity.
        let verdict = policy().evaluate(&Resource::command("git"), PermissionMode::Default);
        assert_eq!(verdict.decision, Decision::Ask);
    }

    #[test]
    fn workspace_files_need_no_prompt() {
        let verdict = policy().evaluate(&Resource::write_file("/proj/src/main.rs"), PermissionMode::Default);
        assert_eq!(verdict.decision, Decision::Allow);
        assert!(matches!(verdict.reason, Reason::WorkspaceDefault));
    }

    #[test]
    fn files_outside_workspace_are_asked() {
        let verdict = policy().evaluate(&Resource::write_file("/etc/hosts"), PermissionMode::Default);
        assert_eq!(verdict.decision, Decision::Ask);
    }

    #[test]
    fn unmatched_actions_fail_closed() {
        let bare = Policy::builder().build().0;
        assert_eq!(bare.evaluate(&Resource::command("whatever"), PermissionMode::Default).decision, Decision::Ask);
    }

    #[test]
    fn plan_mode_refuses_writes_even_when_allowed() {
        let permissive = Policy::builder().workspace_root("/proj").build().0;
        let verdict = permissive.evaluate(&Resource::write_file("/proj/f.rs"), PermissionMode::Plan);
        assert_eq!(verdict.decision, Decision::Deny);
    }

    #[test]
    fn session_grant_stops_repeat_prompts() {
        let mut p = Policy::builder().build().0;
        let resource = Resource::write_file("/outside/f.txt");
        assert_eq!(p.evaluate(&resource, PermissionMode::Default).decision, Decision::Ask);

        p.grant_for_session(&resource).unwrap();
        assert_eq!(p.evaluate(&resource, PermissionMode::Default).decision, Decision::Allow);
        // ...and the implied read is covered too.
        assert_eq!(
            p.evaluate(&Resource::read_file("/outside/f.txt"), PermissionMode::Default).decision,
            Decision::Allow
        );
    }

    #[test]
    fn a_session_grant_suppresses_a_broad_ask_rule() {
        // The regression this guards: with `ask(command(*))` configured, an
        // approved command must not re-prompt on every identical call.
        let mut p = policy();
        let resource = Resource::command("cargo test");
        assert_eq!(p.evaluate(&resource, PermissionMode::Default).decision, Decision::Ask);

        p.grant_for_session(&resource).unwrap();

        let verdict = p.evaluate(&resource, PermissionMode::Default);
        assert_eq!(verdict.decision, Decision::Allow);
        assert!(matches!(verdict.reason, Reason::ExistingGrant { .. }));
    }

    #[test]
    fn a_session_grant_cannot_survive_a_switch_to_plan() {
        let mut p = policy();
        let resource = Resource::command("cargo test");
        p.grant_for_session(&resource).unwrap();

        // Mode is evaluated before session grants, so switching to plan revokes it.
        assert_eq!(p.evaluate(&resource, PermissionMode::Plan).decision, Decision::Deny);
    }

    #[test]
    fn a_session_grant_cannot_override_a_deny_rule() {
        let mut p = policy();
        let resource = Resource::command("rm -rf /");
        // Even if a grant is somehow recorded, deny still wins.
        p.grant_for_session(&resource).unwrap();
        assert_eq!(p.evaluate(&resource, PermissionMode::Default).decision, Decision::Deny);
    }

    #[test]
    fn baseline_denies_protect_git_hooks() {
        let p = Policy::builder()
            .workspace_root("/proj")
            .with_baseline_denies()
            .build()
            .0;
        // Even in bypass, and even though it is inside the workspace.
        let verdict = p.evaluate(&Resource::write_file(".git/hooks/pre-commit"), PermissionMode::Bypass);
        assert_eq!(verdict.decision, Decision::Deny);
    }
}
