//! Kiwi third-party plugins as WebAssembly components (wasmtime + WIT).
//!
//! A plugin exports the `kiwi:plugin` world (`wit/plugin.wit`) and imports nothing, so
//! it has no file system, clock or network: it only sees the atoms the host passes.
//! The host passes an atom only when the plugin declared `content` in its capability
//! manifest *and* the corpus policy allows that plugin to read the atom's access chain
//! (see `PolicyGate::plugin_allows`). Labels are passed only when declared. Every call
//! runs in a fresh store with a fuel budget and a memory cap.
//!
//! Plain core modules built with `wit-bindgen` (for `wasm32-unknown-unknown`) are
//! turned into components when loaded.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::Path;
use std::sync::Arc;

use kiwi_core::model::AccessDecl;
use kiwi_core::plugin::{Capabilities, Enricher, Finding, IndexDoc, Lint, Severity};
use kiwi_core::{KiwiError, Pack};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

mod bindings {
    wasmtime::component::bindgen!({ path: "wit", world: "plugin" });
}

use bindings::kiwi::plugin::types;

/// Decides whether a plugin may read content with an access chain.
pub type Gate = Arc<dyn Fn(&str, &Capabilities, &[AccessDecl]) -> bool + Send + Sync>;

/// Fuel per call: enough for linting thousands of atoms, small enough to stop a loop.
const FUEL: u64 = 2_000_000_000;
/// Memory a plugin may grow to.
const MEMORY_BYTES: usize = 256 * 1024 * 1024;

fn err(plugin: &str, error: impl std::fmt::Display) -> KiwiError {
    KiwiError::plugin(format!("wasm:{plugin}"), error)
}

/// A loaded plugin component.
pub struct WasmPlugin {
    engine: Engine,
    component: Component,
    linker: Linker<StoreLimits>,
    name: String,
    capabilities: Capabilities,
    gate: Option<Gate>,
}

impl std::fmt::Debug for WasmPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmPlugin")
            .field("name", &self.name)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

fn is_component(bytes: &[u8]) -> bool {
    // A component's preamble is `\0asm` with layer 1 (bytes 6..8 = 01 00).
    bytes.len() >= 8 && &bytes[..4] == b"\0asm" && bytes[6..8] == [1, 0]
}

impl WasmPlugin {
    /// Loads a component (or a `wit-bindgen` core module) from bytes, as the plugin
    /// `name`. The host chooses the name (the configuration's), never the component:
    /// the policy governs the plugin as `plugin:<name>`, and a plugin that could name
    /// itself could take the principal of a plugin the policy trusts more.
    pub fn from_bytes(name: &str, bytes: &[u8], gate: Option<Gate>) -> kiwi_core::Result<Self> {
        let label = name;
        if name.is_empty()
            || name.len() > 64
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(err(label, format!("invalid plugin name `{name}`")));
        }
        let component_bytes = if is_component(bytes) {
            bytes.to_vec()
        } else {
            wit_component::ComponentEncoder::default()
                .module(bytes)
                .and_then(|encoder| encoder.validate(true).encode())
                .map_err(|e| err(label, format!("not a kiwi plugin: {e:#}")))?
        };
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|e| err(label, e))?;
        let component = Component::new(&engine, &component_bytes).map_err(|e| err(label, e))?;
        let linker = Linker::new(&engine);
        let mut plugin = Self {
            engine,
            component,
            linker,
            name: label.to_owned(),
            capabilities: Capabilities::default(),
            gate,
        };
        // The component's own `name` export is not consulted.
        let caps = plugin.with_instance(|instance, store| instance.call_capabilities(&mut *store))?;
        plugin.capabilities = Capabilities {
            content: caps.content,
            labels: caps.labels,
            network: caps.network,
        };
        Ok(plugin)
    }

    /// Loads a plugin file as the plugin `name`.
    pub fn load(name: &str, path: &Path, gate: Option<Gate>) -> kiwi_core::Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| KiwiError::io(path, e))?;
        Self::from_bytes(name, &bytes, gate)
    }

    fn with_instance<T>(
        &self,
        run: impl FnOnce(&bindings::Plugin, &mut Store<StoreLimits>) -> wasmtime::Result<T>,
    ) -> kiwi_core::Result<T> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(MEMORY_BYTES)
            .instances(4)
            .build();
        let mut store = Store::new(&self.engine, limits);
        store.limiter(|limits| limits);
        store.set_fuel(FUEL).map_err(|e| err(&self.name, e))?;
        let instance = bindings::Plugin::instantiate(&mut store, &self.component, &self.linker)
            .map_err(|e| err(&self.name, e))?;
        run(&instance, &mut store).map_err(|e| err(&self.name, e))
    }

    /// The plugin's name.
    pub fn plugin_name(&self) -> &str {
        &self.name
    }

    fn allowed(&self, chain: &[AccessDecl]) -> bool {
        if !self.capabilities.content {
            return false;
        }
        match &self.gate {
            Some(gate) => gate(&self.name, &self.capabilities, chain),
            None => true,
        }
    }

    fn atom(&self, doc: &IndexDoc<'_>, chain: &[AccessDecl]) -> types::Atom {
        types::Atom {
            id: doc.id.to_owned(),
            title: doc.title.to_owned(),
            breadcrumb: doc.breadcrumb.to_owned(),
            summary: doc.summary.to_owned(),
            body: doc.body.to_owned(),
            labels: if self.capabilities.labels {
                chain.iter().map(|d| d.label.clone()).collect()
            } else {
                Vec::new()
            },
        }
    }

    /// Runs the plugin's lint over the atoms of `pack` it may read.
    pub fn lint_pack(&self, pack: &Pack) -> kiwi_core::Result<Vec<Finding>> {
        let data = pack.data();
        let mut atoms = Vec::new();
        for uid in 0..pack.atom_count() {
            let Some(atom) = pack.atom(uid) else { continue };
            let chain: Vec<AccessDecl> = data
                .classes
                .get(atom.class.to_native() as usize)
                .map(|class| {
                    class
                        .chain
                        .iter()
                        .map(|d| {
                            AccessDecl::new(d.label.as_str(), d.groups.iter().map(|g| g.as_str().to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !self.allowed(&chain) {
                continue;
            }
            let body = pack.body(uid)?;
            let doc = IndexDoc {
                uid,
                id: &atom.id,
                title: &atom.title,
                breadcrumb: &atom.l0,
                summary: &atom.l1,
                body: &body,
                hash: &[0; 32],
                class: atom.class.to_native(),
                withheld: false,
            };
            atoms.push(self.atom(&doc, &chain));
        }
        let findings = self.with_instance(|instance, store| instance.call_lint(store, &atoms))?;
        Ok(findings
            .into_iter()
            .map(|f| Finding {
                severity: match f.severity {
                    types::Severity::Warning => Severity::Warning,
                    types::Severity::Error => Severity::Error,
                },
                rule: format!("{}/{}", self.name, f.rule),
                id: f.id,
                message: f.message,
            })
            .collect())
    }
}

impl Lint for WasmPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(&self, pack: &Pack) -> kiwi_core::Result<Vec<Finding>> {
        self.lint_pack(pack)
    }
}

impl Enricher for WasmPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn summarize(&self, doc: &IndexDoc<'_>) -> kiwi_core::Result<Option<String>> {
        // The build's plugin gate has already checked this atom's access chain.
        if !self.capabilities.content {
            return Ok(None);
        }
        let atom = self.atom(doc, &[]);
        self.with_instance(|instance, store| instance.call_summarize(store, &atom))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../plugins/example-lint/example_lint.wasm"
        ))
        .unwrap()
    }

    #[test]
    fn loads_the_example_and_reads_its_manifest() {
        let plugin = WasmPlugin::from_bytes("example", &example(), None).unwrap();
        // The host's name, not the one the component reports (`example-lint`).
        assert_eq!(plugin.plugin_name(), "example");
        assert!(plugin.capabilities.content);
        assert!(plugin.capabilities.network.is_empty());
        let hash = [0u8; 32];
        let doc = IndexDoc {
            uid: 0,
            id: "a#b",
            title: "Retries",
            breadcrumb: "A › Retries",
            summary: "Retries",
            body: "Clients retry with backoff. More text follows here.",
            hash: &hash,
            class: 0,
            withheld: false,
        };
        assert_eq!(
            Enricher::summarize(&plugin, &doc).unwrap().as_deref(),
            Some("Clients retry with backoff.")
        );
    }

    #[test]
    fn the_policy_sees_the_configured_name() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&asked);
        let gate: Gate = Arc::new(move |name: &str, _: &Capabilities, _: &[AccessDecl]| {
            log.lock().unwrap().push(name.to_owned());
            false
        });
        let plugin = WasmPlugin::from_bytes("site-lint", &example(), Some(gate)).unwrap();
        assert!(!plugin.allowed(&[AccessDecl::new("internal", Vec::new())]));
        assert_eq!(*asked.lock().unwrap(), vec!["site-lint".to_owned()]);
        assert!(WasmPlugin::from_bytes("bad name", &example(), None).is_err());
    }

    #[test]
    fn rejects_non_plugins() {
        assert!(WasmPlugin::from_bytes("junk", b"not wasm", None).is_err());
        // A valid module that does not export the world.
        let empty = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        assert!(WasmPlugin::from_bytes("empty", &empty, None).is_err());
    }
}
