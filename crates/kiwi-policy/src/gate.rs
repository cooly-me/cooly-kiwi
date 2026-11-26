//! The policy gate: Cedar decides, once per identity and pack, which permission
//! classes a caller may read. The planner then works on the OR of those classes'
//! bitmaps, so no ranking tier ever sees anything else.
//!
//! The effective permission is the intersection of three things (on-behalf-of):
//! what Cedar allows the *user*, the labels the *agent's* grant covers, and the labels
//! the *session* was granted. An agent never sees more than the person it acts for.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;

use cedar_policy::{Authorizer, Context, Decision, Entities, EntityUid, PolicySet, Request};
use kiwi_core::model::AccessDecl;
use kiwi_core::plugin::Capabilities;
use kiwi_core::{Pack, View};
use serde_json::json;

use crate::directory::{Directory, Identity};

/// Errors of the policy gate.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// The Cedar policy does not parse.
    #[error("policy: {0}")]
    Parse(String),
    /// An entity or request could not be built.
    #[error("policy request: {0}")]
    Request(String),
}

/// A Cedar action Kiwi asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Read content with a given access declaration.
    Read,
    /// Propose a change (open a branch and a pull request).
    Propose,
    /// Merge a proposal without review.
    Merge,
}

impl Action {
    fn uid(self) -> &'static str {
        match self {
            Action::Read => r#"Kiwi::Action::"read""#,
            Action::Propose => r#"Kiwi::Action::"propose""#,
            Action::Merge => r#"Kiwi::Action::"merge""#,
        }
    }
}

/// The compiled policy plus the principal directory.
pub struct PolicyGate {
    policies: PolicySet,
    directory: Directory,
    authorizer: Authorizer,
    cache: Mutex<HashMap<(String, Identity), Vec<u32>>>,
}

impl std::fmt::Debug for PolicyGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyGate")
            .field("users", &self.directory.users.len())
            .field("agents", &self.directory.agents.len())
            .finish()
    }
}

fn resource_id(decl: &AccessDecl) -> String {
    format!("{}|{}", decl.label, decl.groups.join(","))
}

impl PolicyGate {
    /// Compiles a Cedar policy source with a directory.
    pub fn new(policy: &str, directory: Directory) -> Result<Self, PolicyError> {
        let policies = PolicySet::from_str(policy).map_err(|e| PolicyError::Parse(e.to_string()))?;
        Ok(Self {
            policies,
            directory,
            authorizer: Authorizer::new(),
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// The directory.
    pub fn directory(&self) -> &Directory {
        &self.directory
    }

    fn entities(&self, identity: &Identity, decl: &AccessDecl) -> Result<Entities, PolicyError> {
        let groups: Vec<String> = self
            .directory
            .user(&identity.principal)
            .map(|u| u.groups.clone())
            .unwrap_or_default();
        let mut entities = vec![json!({
            "uid": { "type": "Kiwi::User", "id": identity.principal },
            "attrs": { "groups": groups },
            "parents": groups.iter().map(|g| json!({ "type": "Kiwi::Group", "id": g })).collect::<Vec<_>>(),
        })];
        for group in &groups {
            entities
                .push(json!({ "uid": { "type": "Kiwi::Group", "id": group }, "attrs": {}, "parents": [] }));
        }
        entities.push(json!({
            "uid": { "type": "Kiwi::Content", "id": resource_id(decl) },
            "attrs": { "label": decl.label, "groups": decl.groups },
            "parents": [],
        }));
        Entities::from_json_value(serde_json::Value::Array(entities), None)
            .map_err(|e| PolicyError::Request(e.to_string()))
    }

    /// Whether Cedar allows `action` for `identity` on content declared `decl`
    /// (before the agent and session limits).
    pub fn cedar_allows(
        &self,
        identity: &Identity,
        action: Action,
        decl: &AccessDecl,
    ) -> Result<bool, PolicyError> {
        let principal = EntityUid::from_str(&format!(
            "Kiwi::User::{}",
            serde_json::Value::String(identity.principal.clone())
        ))
        .map_err(|e| PolicyError::Request(e.to_string()))?;
        let action_uid =
            EntityUid::from_str(action.uid()).map_err(|e| PolicyError::Request(e.to_string()))?;
        let resource = EntityUid::from_str(&format!(
            "Kiwi::Content::{}",
            serde_json::Value::String(resource_id(decl))
        ))
        .map_err(|e| PolicyError::Request(e.to_string()))?;
        let context = Context::from_json_value(
            json!({ "agent": identity.agent, "channel": identity.channel }),
            None,
        )
        .map_err(|e| PolicyError::Request(e.to_string()))?;
        let request = Request::new(principal, action_uid, resource, context, None)
            .map_err(|e| PolicyError::Request(e.to_string()))?;
        let entities = self.entities(identity, decl)?;
        let response = self.authorizer.is_authorized(&request, &self.policies, &entities);
        Ok(response.decision() == Decision::Allow)
    }

    /// Whether the agent's grant and the session's scope cover a label. An agent the
    /// directory does not list has the directory's default grant (none by default).
    fn scopes_allow(&self, identity: &Identity, label: &str) -> bool {
        let agent_ok = self
            .directory
            .agent_labels(&identity.agent)
            .is_none_or(|l| l.iter().any(|x| x == label));
        let session_ok = identity
            .session_labels
            .as_ref()
            .is_none_or(|l| l.iter().any(|x| x == label));
        agent_ok && session_ok
    }

    /// Whether `identity` may read content behind a whole access chain.
    pub fn allows_chain(&self, identity: &Identity, chain: &[AccessDecl]) -> Result<bool, PolicyError> {
        for decl in chain {
            if !self.scopes_allow(identity, &decl.label)
                || !self.cedar_allows(identity, Action::Read, decl)?
            {
                return Ok(false);
            }
        }
        Ok(!chain.is_empty())
    }

    /// The permission classes of `pack` that `identity` may read (cached per pack).
    pub fn classes(&self, pack: &Pack, identity: &Identity) -> Result<Vec<u32>, PolicyError> {
        let key = (pack.id().to_owned(), identity.clone());
        if let Ok(cache) = self.cache.lock()
            && let Some(hit) = cache.get(&key)
        {
            return Ok(hit.clone());
        }
        let mut allowed = Vec::new();
        for (index, class) in pack.data().classes.iter().enumerate() {
            let chain: Vec<AccessDecl> = class
                .chain
                .iter()
                .map(|d| AccessDecl::new(d.label.as_str(), d.groups.iter().map(|g| g.as_str().to_owned())))
                .collect();
            if self.allows_chain(identity, &chain)? {
                allowed.push(index as u32);
            }
        }
        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() > 4096 {
                cache.clear();
            }
            cache.insert(key, allowed.clone());
        }
        Ok(allowed)
    }

    /// The view of `pack` for `identity`: the OR of its permitted class bitmaps.
    pub fn view(&self, pack: &Pack, identity: &Identity) -> Result<View, PolicyError> {
        Ok(View::from_classes(pack, self.classes(pack, identity)?))
    }

    /// Whether the policy alone (no agent grant, no session limit) lets the user of
    /// `identity` do `action` on content behind `chain`; `kiwi policy test` checks this
    /// for cases that name no agent. Writes also need the read.
    pub fn user_allows(
        &self,
        identity: &Identity,
        action: Action,
        chain: &[AccessDecl],
    ) -> Result<bool, PolicyError> {
        for decl in chain {
            if !self.cedar_allows(identity, Action::Read, decl)?
                || (action != Action::Read && !self.cedar_allows(identity, action, decl)?)
            {
                return Ok(false);
            }
        }
        Ok(!chain.is_empty())
    }

    /// Whether `identity` may propose, or merge without review, a change to content
    /// declared `chain` (it must also be able to read it).
    pub fn allows_write(
        &self,
        identity: &Identity,
        action: Action,
        chain: &[AccessDecl],
    ) -> Result<bool, PolicyError> {
        if !self.allows_chain(identity, chain)? {
            return Ok(false);
        }
        for decl in chain {
            if !self.cedar_allows(identity, action, decl)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Governance over plugins: a plugin is treated as the principal
    /// `Kiwi::User::"plugin:<name>"` with `context.agent = "plugin"` and
    /// `context.channel = "network"` when it declares network access, else `"local"`.
    pub fn plugin_allows(&self, name: &str, caps: &Capabilities, chain: &[AccessDecl]) -> bool {
        let identity = Identity::new(
            format!("plugin:{name}"),
            "plugin",
            if caps.network.is_empty() {
                "local"
            } else {
                "network"
            },
        );
        chain
            .iter()
            .all(|decl| self.cedar_allows(&identity, Action::Read, decl).unwrap_or(false))
            && !chain.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const POLICY: &str = r#"
permit (principal, action == Kiwi::Action::"read", resource) when { resource.label == "public" };
permit (principal in Kiwi::Group::"staff", action == Kiwi::Action::"read", resource) when { resource.label == "internal" };
permit (principal, action == Kiwi::Action::"read", resource) when { context.agent == "plugin" && context.channel == "local" };
"#;

    fn gate() -> PolicyGate {
        let directory = Directory::parse(
            "users:\n  - id: alice\n    groups: [staff]\nagents:\n  - id: claude-code\n  - id: narrow\n    labels: [public]\n",
        )
        .unwrap();
        PolicyGate::new(POLICY, directory).unwrap()
    }

    #[test]
    fn chains_need_every_declaration() {
        let gate = gate();
        let alice = Identity::new("alice", "claude-code", "stdio");
        let internal = AccessDecl::new("internal", Vec::new());
        let secret = AccessDecl::new("confidential", ["finance".to_owned()]);
        assert!(
            gate.allows_chain(&alice, std::slice::from_ref(&internal))
                .unwrap()
        );
        assert!(!gate.allows_chain(&alice, &[internal.clone(), secret]).unwrap());
        assert!(
            !gate.allows_chain(&alice, &[]).unwrap(),
            "an empty chain is denied"
        );
        let narrow = Identity::new("alice", "narrow", "stdio");
        assert!(
            !gate
                .allows_chain(&narrow, std::slice::from_ref(&internal))
                .unwrap(),
            "agent grant"
        );
        let mut session = alice.clone();
        session.session_labels = Some(vec!["public".into()]);
        assert!(
            !gate
                .allows_chain(&session, std::slice::from_ref(&internal))
                .unwrap(),
            "session scope"
        );
        // An agent the directory does not list, or a token that names no client,
        // reads nothing: the default grant is empty.
        let public = [AccessDecl::new("public", Vec::new())];
        for agent in ["unlisted-agent", crate::directory::UNIDENTIFIED_AGENT] {
            let stranger = Identity::new("alice", agent, "http");
            assert!(!gate.allows_chain(&stranger, &public).unwrap(), "{agent}");
            assert!(
                !gate
                    .allows_chain(&stranger, std::slice::from_ref(&internal))
                    .unwrap()
            );
        }
        assert!(gate.user_allows(&alice, Action::Read, &[internal]).unwrap());
    }

    #[test]
    fn network_plugins_read_public_only() {
        let gate = gate();
        let local = Capabilities {
            content: true,
            labels: false,
            network: Vec::new(),
        };
        let remote = Capabilities {
            content: true,
            labels: false,
            network: vec!["api.example.com".into()],
        };
        let internal = [AccessDecl::new("internal", Vec::new())];
        let public = [AccessDecl::new("public", Vec::new())];
        assert!(gate.plugin_allows("p", &local, &internal));
        assert!(!gate.plugin_allows("p", &remote, &internal));
        assert!(gate.plugin_allows("p", &remote, &public));
    }
}
