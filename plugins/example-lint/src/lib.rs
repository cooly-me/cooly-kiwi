//! An example Kiwi plugin, compiled to a WebAssembly component.
//!
//! - `lint`: flags `TODO`/`TBD`/`FIXME` markers and sections with almost no text.
//! - `summarize`: for atoms whose first sentence is a bare heading echo, returns the
//!   first sentence of the body instead.
//!
//! Build: `cargo build --release --target wasm32-unknown-unknown` in this directory;
//! the host turns the module into a component when it loads it.

wit_bindgen::generate!({ path: "../../crates/kiwi-wasm/wit", world: "plugin" });

use kiwi::plugin::types::Severity;

struct Plugin;

const MARKERS: [&str; 3] = ["TODO", "TBD", "FIXME"];

impl Guest for Plugin {
    fn name() -> String {
        "example-lint".into()
    }

    fn capabilities() -> Capabilities {
        Capabilities {
            content: true,
            labels: false,
            network: Vec::new(),
        }
    }

    fn lint(atoms: Vec<Atom>) -> Vec<Finding> {
        let mut findings = Vec::new();
        for atom in atoms {
            if let Some(marker) = MARKERS.iter().find(|m| atom.body.contains(*m)) {
                findings.push(Finding {
                    severity: Severity::Warning,
                    rule: "todo-marker".into(),
                    id: atom.id.clone(),
                    message: format!("contains `{marker}`"),
                });
            }
            if atom.body.split_whitespace().count() < 5 {
                findings.push(Finding {
                    severity: Severity::Warning,
                    rule: "thin-section".into(),
                    id: atom.id,
                    message: "fewer than five words".into(),
                });
            }
        }
        findings
    }

    fn summarize(atom: Atom) -> Option<String> {
        if !atom.summary.trim().is_empty() && atom.summary.trim() != atom.title.trim() {
            return None;
        }
        let sentence = atom.body.split(". ").next()?.trim();
        (!sentence.is_empty()).then(|| format!("{}.", sentence.trim_end_matches('.')))
    }
}

export!(Plugin);
