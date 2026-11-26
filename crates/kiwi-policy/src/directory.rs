//! The principal directory: who exists, which groups they belong to, which e-mail a
//! local Git identity maps to, and what each agent's grant allows.
//!
//! ```yaml
//! users:
//!   - id: alice
//!     groups: [eng, staff]
//!     emails: [alice@acme.test]
//! agents:
//!   - id: claude-code
//!   - id: summarizer
//!     labels: [public, internal]   # the most this agent may ever read
//! default_agent:
//!   labels: []                     # an agent not listed reads nothing (the default)
//! ```
//!
//! Agents are an allow-list: one that is not listed (or an HTTP token that names no
//! client) gets `default_agent`'s labels, none unless the directory grants some.

use serde::{Deserialize, Serialize};

/// A person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    /// Stable id, the Cedar principal `Kiwi::User::"<id>"`.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Groups, each also a Cedar parent `Kiwi::Group::"<group>"`.
    #[serde(default)]
    pub groups: Vec<String>,
    /// E-mail addresses (matched against `git config user.email` for stdio, a
    /// convenience for the local user, not an authentication).
    #[serde(default)]
    pub emails: Vec<String>,
}

/// An agent (MCP client) and the most it may read on anyone's behalf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    /// Agent id, passed to Cedar as `context.agent`.
    pub id: String,
    /// Labels the agent's grant covers; `None` means no extra limit.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
}

/// The grant of an agent the directory does not list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultAgent {
    /// The labels it may read (none by default).
    pub labels: Vec<String>,
}

/// The agent id of an HTTP caller whose token names no client (`client_id` or `azp`).
/// No directory entry may use it, so such a caller always gets `default_agent`.
pub const UNIDENTIFIED_AGENT: &str = "(unidentified)";

/// The directory file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directory {
    /// People.
    #[serde(default)]
    pub users: Vec<User>,
    /// Agents, an allow-list.
    #[serde(default)]
    pub agents: Vec<Agent>,
    /// The grant of any agent not in `agents`; absent, such an agent reads nothing.
    #[serde(default)]
    pub default_agent: DefaultAgent,
}

/// Errors of the directory file.
#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    /// Not valid YAML for a directory.
    #[error(transparent)]
    Yaml(#[from] serde_yaml_ng::Error),
    /// An entry is not allowed.
    #[error("{0}")]
    Invalid(String),
}

impl Directory {
    /// Parses YAML.
    pub fn parse(source: &str) -> Result<Self, DirectoryError> {
        let directory: Self = serde_yaml_ng::from_str(source)?;
        for agent in &directory.agents {
            if agent.id.trim().is_empty() || agent.id == UNIDENTIFIED_AGENT {
                return Err(DirectoryError::Invalid(format!(
                    "agent id `{}` is reserved or empty",
                    agent.id
                )));
            }
        }
        Ok(directory)
    }

    /// The labels an agent's grant covers: `None` when its grant sets no limit, the
    /// default agent's labels when it is not listed.
    pub fn agent_labels(&self, id: &str) -> Option<&[String]> {
        match self.agent(id) {
            Some(agent) => agent.labels.as_deref(),
            None => Some(&self.default_agent.labels),
        }
    }

    /// A user by id.
    pub fn user(&self, id: &str) -> Option<&User> {
        self.users.iter().find(|u| u.id == id)
    }

    /// A user by e-mail (case-insensitive).
    pub fn user_by_email(&self, email: &str) -> Option<&User> {
        self.users
            .iter()
            .find(|u| u.emails.iter().any(|e| e.eq_ignore_ascii_case(email.trim())))
    }

    /// An agent by id.
    pub fn agent(&self, id: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id == id)
    }
}

/// Who is asking: the person, the agent acting for them, and the session's scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Identity {
    /// The user id.
    pub principal: String,
    /// The agent id (the MCP client).
    pub agent: String,
    /// `stdio` or `http`.
    pub channel: String,
    /// Labels the session was granted (OAuth scopes `kiwi:label:<label>`); `None` means
    /// the session adds no limit.
    pub session_labels: Option<Vec<String>>,
}

impl Identity {
    /// An identity with no session limit.
    pub fn new(principal: impl Into<String>, agent: impl Into<String>, channel: impl Into<String>) -> Self {
        Self {
            principal: principal.into(),
            agent: agent.into(),
            channel: channel.into(),
            session_labels: None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn lookups() {
        let dir = Directory::parse(
            "users:\n  - id: alice\n    groups: [eng]\n    emails: [Alice@Acme.test]\nagents:\n  - id: bot\n    labels: [public]\n",
        )
        .unwrap();
        assert_eq!(dir.user_by_email("alice@acme.test").unwrap().id, "alice");
        assert!(dir.user("bob").is_none());
        assert_eq!(
            dir.agent("bot").unwrap().labels.as_ref().unwrap(),
            &vec!["public".to_owned()]
        );
        assert!(Directory::parse("users:\n  - id: a\n    unknown: 1\n").is_err());
        // A public key proves nothing about who runs the process: not accepted.
        assert!(Directory::parse("users:\n  - id: a\n    ssh_keys: [\"SHA256:x\"]\n").is_err());
    }

    #[test]
    fn unlisted_agents_get_the_default_grant() {
        let dir = Directory::parse("agents:\n  - id: open\n  - id: bot\n    labels: [public]\n").unwrap();
        assert_eq!(dir.agent_labels("open"), None);
        assert_eq!(dir.agent_labels("bot"), Some(&["public".to_owned()][..]));
        assert_eq!(dir.agent_labels("stranger"), Some(&[][..]), "deny by default");
        assert_eq!(dir.agent_labels(UNIDENTIFIED_AGENT), Some(&[][..]));
        let lenient = Directory::parse("default_agent:\n  labels: [public]\n").unwrap();
        assert_eq!(lenient.agent_labels("stranger"), Some(&["public".to_owned()][..]));
        assert!(Directory::parse("agents:\n  - id: \"(unidentified)\"\n").is_err());
        assert!(
            Directory::parse("default_agent: {}\n").is_err(),
            "labels are required"
        );
    }
}
