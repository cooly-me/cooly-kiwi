//! Who is calling over stdio.
//!
//! A stdio server runs as the local user who started it, for that user's agent, and
//! the user is whoever that process says it is: `--principal` (or `KIWI_PRINCIPAL`),
//! else the repository's `git config user.email` matched against the directory. That
//! is an assertion, not an authentication: anyone who can start the process can pass
//! any principal, and can already read the corpus's files. Governance over stdio keeps
//! an agent within what its user (and the agent's own grant) may read; it does not
//! protect content from the person at the keyboard. Callers who must prove who they
//! are use HTTP with OAuth tokens, and a policy can trust the `http` channel more.

use std::path::Path;
use std::process::Command;

use kiwi_policy::{Directory, Identity};

fn git_email(corpus: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "user.email"])
        .current_dir(corpus)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|e| !e.is_empty())
}

/// Resolves the stdio caller. Returns the identity and how it was found.
pub fn resolve(
    directory: &Directory,
    corpus: &Path,
    principal: Option<&str>,
    agent: &str,
) -> (Identity, String) {
    if let Some(principal) = principal {
        return (Identity::new(principal, agent, "stdio"), "--principal".into());
    }
    if let Some(email) = git_email(corpus)
        && let Some(user) = directory.user_by_email(&email)
    {
        return (
            Identity::new(user.id.clone(), agent, "stdio"),
            format!("git user.email {email}"),
        );
    }
    (
        Identity::new("anonymous", agent, "stdio"),
        "no matching identity".into(),
    )
}
