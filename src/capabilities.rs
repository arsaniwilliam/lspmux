//! What each client declared it supports and which dynamic registrations it
//! received
//!
//! The server is initialized once with the first client's capabilities, but
//! the clients sharing it may differ. Server requests a client didn't declare
//! support for are only sent to the clients which did.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::lsp::{self, Registration, Unregistration};
use crate::settings::lookup;

/// A client's `initialize` request capabilities and info, kept for as long
/// as it's connected
pub struct ClientRecord {
    pub capabilities: Value,
    pub client_info: Option<lsp::ClientInfo>,
}

impl ClientRecord {
    pub fn new(params: &lsp::InitializeParams) -> ClientRecord {
        ClientRecord {
            capabilities: params.capabilities.clone().unwrap_or_else(|| json!({})),
            client_info: params.client_info.clone(),
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.client_info.as_ref().map(|info| info.name.as_str())
    }

    /// Is the capability at the dotted `path` declared `true`
    pub fn declares(&self, path: &str) -> bool {
        lookup(&self.capabilities, path) == Some(&Value::Bool(true))
    }
}

/// Capabilities lspmux declares to the server regardless of the first client
///
/// The mux can always honour them itself:
/// - `workspace.configuration`: answered from `server_settings`, by a client
///   declaring it, or with `null`s
/// - `workspace.didChangeWatchedFiles.dynamicRegistration`: registrations are
///   answered by lspmux and forwarded to the clients declaring it
/// - `window.workDoneProgress`: `window/workDoneProgress/create` is answered
///   by lspmux, progress is replayed to clients attaching later
/// - `textDocument.publishDiagnostics.versionSupport`: lspmux needs versioned
///   diagnostics to tell which client's text they're for and translates them
///   to each client's numbering
pub fn force_mux_capabilities(capabilities: &mut Option<Value>) {
    let capabilities = capabilities.get_or_insert_with(|| json!({}));
    for path in [
        "workspace.configuration",
        "workspace.didChangeWatchedFiles.dynamicRegistration",
        "window.workDoneProgress",
        "textDocument.publishDiagnostics.versionSupport",
    ] {
        set_path(capabilities, path, Value::Bool(true));
    }
}

/// Set the value at the dotted `path`, replacing anything which isn't an
/// object on the way
fn set_path(value: &mut Value, path: &str, new: Value) {
    let mut keys = path.split('.').peekable();
    let mut value = value;
    while let Some(key) = keys.next() {
        if !value.is_object() {
            *value = json!({});
        }
        let object = value.as_object_mut().unwrap();
        if keys.peek().is_none() {
            object.insert(key.to_owned(), new);
            return;
        }
        value = object.entry(key).or_insert_with(|| json!({}));
    }
}

/// Client capability declaring dynamic registration support for a
/// registration `method`, `None` for methods lspmux doesn't know
pub fn registration_capability(method: &str) -> Option<String> {
    let capability = match method {
        "textDocument/didOpen"
        | "textDocument/didChange"
        | "textDocument/didClose"
        | "textDocument/willSave"
        | "textDocument/willSaveWaitUntil"
        | "textDocument/didSave" => "textDocument.synchronization".to_owned(),
        "workspace/willCreateFiles"
        | "workspace/didCreateFiles"
        | "workspace/willRenameFiles"
        | "workspace/didRenameFiles"
        | "workspace/willDeleteFiles"
        | "workspace/didDeleteFiles" => "workspace.fileOperations".to_owned(),
        "notebookDocument/sync" => "notebookDocument.synchronization".to_owned(),
        _ => {
            let (scope, feature) = method.split_once('/')?;
            if !matches!(scope, "textDocument" | "workspace") || feature.contains('/') {
                return None;
            }
            format!("{scope}.{feature}")
        }
    };
    Some(format!("{capability}.dynamicRegistration"))
}

/// Client capability declaring support for a `workspace/<feature>/refresh`
/// server request
pub fn refresh_capability(method: &str) -> Option<String> {
    let feature = method
        .strip_prefix("workspace/")?
        .strip_suffix("/refresh")?;
    // The only one whose client capability is in plural
    let feature = if feature == "diagnostic" {
        "diagnostics"
    } else {
        feature
    };
    Some(format!("workspace.{feature}.refreshSupport"))
}

/// Server requests answered by lspmux and forwarded only to clients
/// declaring support for them
pub const REFRESH_REQUESTS: [&str; 6] = [
    "workspace/semanticTokens/refresh",
    "workspace/codeLens/refresh",
    "workspace/inlayHint/refresh",
    "workspace/inlineValue/refresh",
    "workspace/diagnostic/refresh",
    "workspace/foldingRange/refresh",
];

const WATCHED_FILES: &str = "workspace/didChangeWatchedFiles";

fn accepts(record: &ClientRecord, method: &str) -> bool {
    registration_capability(method).is_none_or(|capability| record.declares(&capability))
}

struct Registered {
    registration: Registration,
    /// Clients which were sent the registration
    receivers: Vec<usize>,
}

/// Dynamic capabilities registered by the server, and the connected clients
#[derive(Default)]
pub struct Registrations {
    /// In registration order
    entries: Vec<Registered>,
    /// Connected clients in connect order
    clients: Vec<(usize, Arc<ClientRecord>)>,
}

impl Registrations {
    /// Add a connected client, returns the registrations it must be sent
    pub fn add_client(&mut self, client_id: usize, record: Arc<ClientRecord>) -> Vec<Registration> {
        let replay = self
            .entries
            .iter_mut()
            .filter(|entry| accepts(&record, &entry.registration.method))
            .map(|entry| {
                entry.receivers.push(client_id);
                entry.registration.clone()
            })
            .collect();
        self.clients.push((client_id, record));
        replay
    }

    pub fn remove_client(&mut self, client_id: usize) {
        self.clients.retain(|(id, _)| *id != client_id);
        for entry in &mut self.entries {
            entry.receivers.retain(|&id| id != client_id);
        }
    }

    /// Record server registrations, returns what each client must be sent
    pub fn register(
        &mut self,
        registrations: Vec<Registration>,
    ) -> Vec<(usize, Vec<Registration>)> {
        let mut deliveries = Vec::<(usize, Vec<Registration>)>::new();
        for registration in registrations {
            let receivers = self
                .clients
                .iter()
                .filter(|(_, record)| accepts(record, &registration.method))
                .map(|&(id, _)| id)
                .collect::<Vec<_>>();
            for &client_id in &receivers {
                push_to(&mut deliveries, client_id, registration.clone());
            }
            self.entries
                .retain(|entry| entry.registration.id != registration.id);
            self.entries.push(Registered {
                registration,
                receivers,
            });
        }
        deliveries
    }

    /// Forget server registrations, returns what each client must be sent:
    /// only what it received earlier
    pub fn unregister(
        &mut self,
        unregistrations: Vec<Unregistration>,
    ) -> Vec<(usize, Vec<Unregistration>)> {
        let mut deliveries = Vec::<(usize, Vec<Unregistration>)>::new();
        for unregistration in unregistrations {
            let Some(index) = self
                .entries
                .iter()
                .position(|entry| entry.registration.id == unregistration.id)
            else {
                continue;
            };
            for client_id in self.entries.remove(index).receivers {
                push_to(&mut deliveries, client_id, unregistration.clone());
            }
        }
        deliveries
    }

    /// The client whose `workspace/didChangeWatchedFiles` reach the server:
    /// the earliest connected one which received a watched files
    /// registration
    ///
    /// `None` when nobody did, notifications from every client are forwarded
    /// then, they're the clients' own decision.
    pub fn watcher(&self) -> Option<usize> {
        self.clients.iter().map(|&(id, _)| id).find(|&id| {
            self.entries.iter().any(|entry| {
                entry.registration.method == WATCHED_FILES && entry.receivers.contains(&id)
            })
        })
    }

    /// Should `workspace/didChangeWatchedFiles` from `client_id` reach the
    /// server
    pub fn forwards_watched_files_from(&self, client_id: usize) -> bool {
        self.watcher().is_none_or(|watcher| watcher == client_id)
    }

    /// Registered methods
    pub fn methods(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.registration.method.clone())
            .collect()
    }
}

fn push_to<T>(deliveries: &mut Vec<(usize, Vec<T>)>, client_id: usize, item: T) {
    match deliveries.iter_mut().find(|(id, _)| *id == client_id) {
        Some((_, items)) => items.push(item),
        None => deliveries.push((client_id, vec![item])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVIM: usize = 1;
    const OMP: usize = 2;
    const OTHER: usize = 3;

    fn record(capabilities: Value) -> Arc<ClientRecord> {
        Arc::new(ClientRecord {
            capabilities,
            client_info: None,
        })
    }

    /// Declares dynamic registration of watched files and hover
    fn watching() -> Arc<ClientRecord> {
        record(json!({
            "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } },
            "textDocument": { "hover": { "dynamicRegistration": true } },
        }))
    }

    fn static_only() -> Arc<ClientRecord> {
        record(json!({
            "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": false } },
            "textDocument": { "synchronization": { "dynamicRegistration": false } },
        }))
    }

    fn registration(id: &str, method: &str) -> Registration {
        Registration {
            id: id.into(),
            method: method.into(),
            register_options: None,
        }
    }

    fn unregistration(id: &str, method: &str) -> Unregistration {
        Unregistration {
            id: id.into(),
            method: method.into(),
        }
    }

    fn ids<T>(
        deliveries: &[(usize, Vec<T>)],
        id: impl Fn(&T) -> &str,
    ) -> Vec<(usize, Vec<String>)> {
        deliveries
            .iter()
            .map(|(client_id, items)| {
                (
                    *client_id,
                    items.iter().map(|item| id(item).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn capability_paths() {
        assert_eq!(
            registration_capability("workspace/didChangeWatchedFiles").as_deref(),
            Some("workspace.didChangeWatchedFiles.dynamicRegistration")
        );
        assert_eq!(
            registration_capability("textDocument/hover").as_deref(),
            Some("textDocument.hover.dynamicRegistration")
        );
        assert_eq!(
            registration_capability("textDocument/didSave").as_deref(),
            Some("textDocument.synchronization.dynamicRegistration")
        );
        assert_eq!(registration_capability("haskell/custom"), None);
        assert_eq!(registration_capability("textDocument/a/b"), None);

        assert_eq!(
            refresh_capability("workspace/semanticTokens/refresh").as_deref(),
            Some("workspace.semanticTokens.refreshSupport")
        );
        assert_eq!(
            refresh_capability("workspace/diagnostic/refresh").as_deref(),
            Some("workspace.diagnostics.refreshSupport")
        );
        for method in REFRESH_REQUESTS {
            assert!(refresh_capability(method).is_some(), "{method}");
        }
    }

    #[test]
    fn forced_server_capabilities() {
        let mut capabilities = Some(json!({
            "window": { "workDoneProgress": false, "showDocument": { "support": true } },
            "workspace": { "didChangeWatchedFiles": true },
        }));
        force_mux_capabilities(&mut capabilities);
        let forced = record(capabilities.unwrap());
        for path in [
            "workspace.configuration",
            "workspace.didChangeWatchedFiles.dynamicRegistration",
            "window.workDoneProgress",
            "textDocument.publishDiagnostics.versionSupport",
            // kept
            "window.showDocument.support",
        ] {
            assert!(forced.declares(path), "{path}");
        }

        let mut capabilities = None;
        force_mux_capabilities(&mut capabilities);
        assert!(record(capabilities.unwrap()).declares("window.workDoneProgress"));
    }

    #[test]
    fn registrations_filtered_by_capabilities() {
        let mut registrations = Registrations::default();
        assert!(registrations.add_client(NVIM, watching()).is_empty());
        assert!(registrations.add_client(OMP, static_only()).is_empty());

        let deliveries = registrations.register(vec![
            registration("w", WATCHED_FILES),
            registration("h", "textDocument/hover"),
            registration("s", "textDocument/didSave"),
            registration("x", "haskell/custom"),
        ]);
        // unknown methods reach everyone
        assert_eq!(
            ids(&deliveries, |r| &r.id),
            [
                (NVIM, vec!["w".into(), "h".into(), "x".into()]),
                (OMP, vec!["x".into()])
            ]
        );

        // replay to later clients is filtered the same way
        let replay = registrations.add_client(OTHER, static_only());
        let replay = replay.iter().map(|r| r.id.as_str()).collect::<Vec<_>>();
        assert_eq!(replay, ["x"]);
        let replay = registrations.add_client(4, watching());
        let replay = replay.iter().map(|r| r.id.as_str()).collect::<Vec<_>>();
        assert_eq!(replay, ["w", "h", "x"]);

        // unregistrations only go to receivers, unknown ids to nobody
        let deliveries = registrations.unregister(vec![
            unregistration("w", WATCHED_FILES),
            unregistration("x", "haskell/custom"),
            unregistration("nope", "textDocument/hover"),
        ]);
        assert_eq!(
            ids(&deliveries, |u| &u.id),
            [
                (NVIM, vec!["w".into(), "x".into()]),
                (4, vec!["w".into(), "x".into()]),
                (OMP, vec!["x".into()]),
                (OTHER, vec!["x".into()]),
            ]
        );
        assert_eq!(
            registrations.methods(),
            ["textDocument/hover", "textDocument/didSave"]
        );
    }

    #[test]
    fn watcher_election_and_hand_over() {
        let mut registrations = Registrations::default();
        registrations.add_client(OMP, static_only());
        // no watched files registration yet, nothing to deduplicate
        assert_eq!(registrations.watcher(), None);
        assert!(registrations.forwards_watched_files_from(OMP));

        registrations.add_client(NVIM, watching());
        registrations.add_client(OTHER, watching());
        registrations.register(vec![registration("w", WATCHED_FILES)]);
        // OMP connected first but didn't receive it
        assert_eq!(registrations.watcher(), Some(NVIM));
        assert!(registrations.forwards_watched_files_from(NVIM));
        assert!(!registrations.forwards_watched_files_from(OMP));
        assert!(!registrations.forwards_watched_files_from(OTHER));

        // hand over to the next eligible client
        registrations.remove_client(NVIM);
        assert_eq!(registrations.watcher(), Some(OTHER));
        assert!(!registrations.forwards_watched_files_from(OMP));

        // a later watching client is eligible from its replay on
        registrations.add_client(4, watching());
        registrations.remove_client(OTHER);
        assert_eq!(registrations.watcher(), Some(4));

        // nobody watching anymore, every client's own notifications pass
        registrations.remove_client(4);
        assert_eq!(registrations.watcher(), None);
        assert!(registrations.forwards_watched_files_from(OMP));
    }
}
