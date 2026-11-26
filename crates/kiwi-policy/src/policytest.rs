//! `kiwi policy test`: unit tests for the Cedar policy, run in CI.
//!
//! ```yaml
//! tests:
//!   - name: contractors only read public content
//!     principal: carl
//!     agent: claude-code      # optional: without it, the policy alone is tested
//!     channel: http           # stdio | http (default stdio)
//!     action: read            # read | propose | merge (default read)
//!     label: internal
//!     groups: []
//!     expect: deny
//! ```

use kiwi_core::model::AccessDecl;
use serde::Deserialize;

use crate::directory::Identity;
use crate::gate::{Action, PolicyError, PolicyGate};

/// One policy test case.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyCase {
    /// Name shown in the report.
    pub name: String,
    /// The user id.
    pub principal: String,
    /// The agent id; with one, its grant in the directory applies too.
    #[serde(default)]
    pub agent: Option<String>,
    /// `stdio` or `http`.
    #[serde(default = "default_channel")]
    pub channel: String,
    /// `read`, `propose` or `merge`.
    #[serde(default = "default_action")]
    pub action: String,
    /// Content label.
    pub label: String,
    /// Content groups.
    #[serde(default)]
    pub groups: Vec<String>,
    /// `allow` or `deny`.
    pub expect: String,
}

fn default_channel() -> String {
    "stdio".into()
}

/// The agent Cedar sees in a case that names none.
const NO_AGENT: &str = "test";

fn default_action() -> String {
    "read".into()
}

/// The test file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyTests {
    /// Cases.
    pub tests: Vec<PolicyCase>,
}

/// The outcome of one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseResult {
    /// Case name.
    pub name: String,
    /// Whether the decision matched.
    pub passed: bool,
    /// The decision.
    pub actual: String,
    /// The expectation.
    pub expected: String,
}

/// Runs every case against the gate (agent grants included).
pub fn run(gate: &PolicyGate, tests: &PolicyTests) -> Result<Vec<CaseResult>, PolicyError> {
    let mut results = Vec::with_capacity(tests.tests.len());
    for case in &tests.tests {
        let action = match case.action.as_str() {
            "read" => Action::Read,
            "propose" => Action::Propose,
            "merge" => Action::Merge,
            other => return Err(PolicyError::Request(format!("unknown action `{other}`"))),
        };
        let agent = case.agent.as_deref().unwrap_or(NO_AGENT);
        let identity = Identity::new(case.principal.clone(), agent, case.channel.clone());
        let decl = AccessDecl::new(case.label.clone(), case.groups.clone());
        let chain = std::slice::from_ref(&decl);
        let allowed = match (action, &case.agent) {
            (_, None) => gate.user_allows(&identity, action, chain)?,
            (Action::Read, Some(_)) => gate.allows_chain(&identity, chain)?,
            (other, Some(_)) => gate.allows_write(&identity, other, chain)?,
        };
        let actual = if allowed { "allow" } else { "deny" }.to_owned();
        results.push(CaseResult {
            name: case.name.clone(),
            passed: actual == case.expect,
            actual,
            expected: case.expect.clone(),
        });
    }
    Ok(results)
}
