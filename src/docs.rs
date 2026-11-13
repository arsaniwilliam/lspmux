//! Document synchronization between many clients and one language server
//!
//! Every client keeps its own copy of each open document, the language server
//! however only knows a single copy per URI. This module tracks all client
//! copies and the server copy and decides which `textDocument/did*`
//! notifications the server needs so that its copy matches the text of the
//! client currently interacting with it. It also caches
//! `textDocument/publishDiagnostics` and decides which client may see them.
//!
//! Everything here is pure state manipulation, the caller is responsible for
//! actually sending the produced messages.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::lsp::jsonrpc::{Notification, Version};

/// `TextDocumentSyncKind.Full`
const SYNC_FULL: u64 = 1;

/// `TextDocumentSyncKind.Incremental`
const SYNC_INCREMENTAL: u64 = 2;

/// Negotiated `PositionEncodingKind`, determines the unit of `Position.character`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    /// Read `positionEncoding` from server capabilities, defaults to UTF-16
    pub fn from_server_capabilities(capabilities: &Value) -> PositionEncoding {
        match capabilities.get("positionEncoding").and_then(Value::as_str) {
            Some("utf-8") => PositionEncoding::Utf8,
            Some("utf-32") => PositionEncoding::Utf32,
            _ => PositionEncoding::Utf16,
        }
    }

    fn units(self, ch: char) -> u64 {
        match self {
            PositionEncoding::Utf8 => ch.len_utf8() as u64,
            PositionEncoding::Utf16 => ch.len_utf16() as u64,
            PositionEncoding::Utf32 => 1,
        }
    }
}

/// Make clients send full text `textDocument/didChange` notifications
///
/// Rewrites `textDocumentSync` in server capabilities from `Incremental` to
/// `Full`, both in the number form and the `TextDocumentSyncOptions` object
/// form. Other options are kept as they are, so is `None`, a server which
/// doesn't want changes won't get any.
pub fn rewrite_text_document_sync(capabilities: &mut Value) {
    let Some(sync) = capabilities.get_mut("textDocumentSync") else {
        return;
    };
    let change = match sync {
        Value::Number(_) => sync,
        Value::Object(options) => match options.get_mut("change") {
            Some(change) => change,
            None => return,
        },
        _ => return,
    };
    if change.as_u64() == Some(SYNC_INCREMENTAL) {
        *change = SYNC_FULL.into();
    }
}

/// Convert an LSP `Position` into a byte offset into `text`
///
/// Positions past the end of a line resolve to the end of that line, lines
/// past the end of the document resolve to the end of the document, as the
/// specification asks.
fn offset_at(text: &str, position: &Value, encoding: PositionEncoding) -> Result<usize> {
    let line = position["line"].as_u64().context("invalid position line")?;
    let character = position["character"]
        .as_u64()
        .context("invalid position character")?;

    let mut line_start = 0;
    for _ in 0..line {
        let rest = &text[line_start..];
        let Some(terminator) = rest.find(['\n', '\r']) else {
            return Ok(text.len());
        };
        line_start += terminator + 1;
        if rest[terminator..].starts_with("\r\n") {
            line_start += 1;
        }
    }

    let rest = &text[line_start..];
    let line_text = &rest[..rest.find(['\n', '\r']).unwrap_or(rest.len())];
    let mut units = 0;
    for (index, ch) in line_text.char_indices() {
        if units >= character {
            return Ok(line_start + index);
        }
        units += encoding.units(ch);
    }
    Ok(line_start + line_text.len())
}

/// Apply one `TextDocumentContentChangeEvent` to `text`
pub fn apply_change(text: &mut String, change: &Value, encoding: PositionEncoding) -> Result<()> {
    let new_text = change["text"].as_str().context("change without text")?;
    match change.get("range") {
        None | Some(Value::Null) => {
            new_text.clone_into(text);
        }
        Some(range) => {
            let start = offset_at(text, &range["start"], encoding)?;
            let end = offset_at(text, &range["end"], encoding)?;
            if start > end {
                bail!("change range ends before it starts");
            }
            text.replace_range(start..end, new_text);
        }
    }
    Ok(())
}

/// Result of a document event: messages for the server and for clients
#[derive(Default, Debug)]
pub struct Effects {
    pub to_server: Vec<Notification>,
    pub to_clients: Vec<(usize, Notification)>,
}

/// One client's copy of a document
struct ClientDoc {
    text: String,
    /// Latest version number the client used for this document
    version: i64,
    /// Logical time of the last interaction of the client with this document
    touched: u64,
    /// Latest server version whose text equals this client's current text
    matched_server_version: Option<i64>,
}

/// The language server's copy of a document
///
/// The server has the document open for as long as the [`Doc`] exists.
struct ServerDoc {
    text: String,
    /// Version numbering owned by lspmux, strictly increasing
    version: i64,
    /// Client whose text was last sent to the server
    synced_from: usize,
    /// Changes whenever the server text switches to text which came from
    /// another client than `synced_from`, values are unique per [`Docs`]
    foreign_epoch: u64,
}

struct Doc {
    language_id: String,
    clients: HashMap<usize, ClientDoc>,
    server: ServerDoc,
}

impl Doc {
    /// Version `client_id` uses for the server's current text, if it has the
    /// same text open
    fn client_version_of_server_text(&self, client_id: usize) -> Option<i64> {
        self.clients
            .get(&client_id)
            .filter(|client| client.text == self.server.text)
            .map(|client| client.version)
    }

    /// `client_id`'s version of the text the server had at `server_version`,
    /// `None` when the client doesn't have that text (anymore)
    fn translate(&self, client_id: usize, server_version: i64) -> Option<i64> {
        self.clients
            .get(&client_id)
            .filter(|client| client.matched_server_version == Some(server_version))
            .map(|client| client.version)
    }
}

/// Document state of a client at the time it sent a request to the server
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Foreign epoch and client version per URI the client had open
    docs: HashMap<String, (u64, i64)>,
}

/// Reads the current disk text of a `file://` URI
pub type ReadUri<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Document and diagnostics state of one language server instance
pub struct Docs {
    encoding: PositionEncoding,
    docs: HashMap<String, Doc>,
    /// Last server version of documents which were closed, reopening
    /// continues the numbering so old versioned messages can never be mistaken
    /// for current ones.
    closed_versions: HashMap<String, i64>,
    /// Latest non-empty `textDocument/publishDiagnostics` params per URI
    diagnostics: HashMap<String, Value>,
    /// Logical clock for [`ClientDoc::touched`]
    clock: u64,
    /// Source of [`ServerDoc::foreign_epoch`] values
    epoch_clock: u64,
}

fn notification(method: &str, params: Value) -> Notification {
    Notification {
        jsonrpc: Version,
        method: method.into(),
        params,
    }
}

fn full_change(uri: &str, version: i64, text: &str) -> Notification {
    notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{ "text": text }],
        }),
    )
}

fn publish_diagnostics(params: Value) -> Notification {
    notification("textDocument/publishDiagnostics", params)
}

fn text_document_uri(params: &Value) -> Result<&str> {
    params["textDocument"]["uri"]
        .as_str()
        .context("missing textDocument.uri")
}

impl Docs {
    pub fn new(encoding: PositionEncoding) -> Docs {
        Docs {
            encoding,
            docs: HashMap::new(),
            closed_versions: HashMap::new(),
            diagnostics: HashMap::new(),
            clock: 0,
            epoch_clock: 0,
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn next_epoch(&mut self) -> u64 {
        self.epoch_clock += 1;
        self.epoch_clock
    }

    /// Does `client_id` have `uri` open
    pub fn has_open(&self, client_id: usize, uri: &str) -> bool {
        self.docs
            .get(uri)
            .is_some_and(|doc| doc.clients.contains_key(&client_id))
    }

    /// URIs `client_id` has open
    pub fn files_of(&self, client_id: usize) -> Vec<String> {
        self.docs
            .iter()
            .filter(|(_, doc)| doc.clients.contains_key(&client_id))
            .map(|(uri, _)| uri.clone())
            .collect()
    }

    /// Make the server text of `uri` equal `client_id`'s text
    fn resync(&mut self, uri: &str, client_id: usize) -> Option<Notification> {
        let doc = self.docs.get_mut(uri)?;
        let client = doc.clients.get(&client_id)?;
        if client.text == doc.server.text {
            return None;
        }
        doc.server.version += 1;
        doc.server.text.clone_from(&client.text);
        if doc.server.synced_from != client_id {
            self.epoch_clock += 1;
            doc.server.foreign_epoch = self.epoch_clock;
        }
        doc.server.synced_from = client_id;
        for client in doc.clients.values_mut() {
            if client.text == doc.server.text {
                client.matched_server_version = Some(doc.server.version);
            }
        }
        Some(full_change(uri, doc.server.version, &doc.server.text))
    }

    /// Diagnostics `params` as `client_id` should see them, `None` means
    /// they must be withheld
    fn diagnostics_for(&self, client_id: usize, params: &Value) -> Option<Value> {
        let uri = params["uri"].as_str()?;
        let Some(doc) = self.docs.get(uri).filter(|doc| doc.clients.contains_key(&client_id))
        else {
            // Not opened by the client, versions mean nothing to it.
            return Some(params.clone());
        };
        let client_version = match params.get("version") {
            None | Some(Value::Null) => doc.client_version_of_server_text(client_id)?,
            Some(version) => doc.translate(client_id, version.as_i64()?)?,
        };
        let mut params = params.clone();
        params["version"] = client_version.into();
        Some(params)
    }

    /// Handle `textDocument/didOpen` from `client_id`
    pub fn did_open(&mut self, client_id: usize, params: &Value) -> Result<Effects> {
        let item = &params["textDocument"];
        let uri = item["uri"].as_str().context("missing textDocument.uri")?;
        let text = item["text"].as_str().context("missing textDocument.text")?;
        let version = item["version"]
            .as_i64()
            .context("missing textDocument.version")?;
        let language_id = item["languageId"].as_str().unwrap_or_default();

        let touched = self.tick();
        let mut client = ClientDoc {
            text: text.to_owned(),
            version,
            touched,
            matched_server_version: None,
        };

        let mut effects = Effects::default();
        if let Some(doc) = self.docs.get_mut(uri) {
            if client.text == doc.server.text {
                client.matched_server_version = Some(doc.server.version);
            }
            doc.clients.insert(client_id, client);
            effects.to_server.extend(self.resync(uri, client_id));
        } else {
            let server_version = self.closed_versions.remove(uri).map_or(1, |v| v + 1);
            client.matched_server_version = Some(server_version);
            let foreign_epoch = self.next_epoch();
            let doc = Doc {
                language_id: language_id.to_owned(),
                clients: HashMap::from([(client_id, client)]),
                server: ServerDoc {
                    text: text.to_owned(),
                    version: server_version,
                    synced_from: client_id,
                    foreign_epoch,
                },
            };
            effects.to_server.push(notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": doc.language_id,
                        "version": doc.server.version,
                        "text": doc.server.text,
                    },
                }),
            ));
            self.docs.insert(uri.to_owned(), doc);
        }

        // The client might be showing cached diagnostics replayed on connect,
        // either correct them or clear them if they're for another text.
        if let Some(cached) = self.diagnostics.get(uri) {
            let params = self
                .diagnostics_for(client_id, cached)
                .unwrap_or_else(|| json!({ "uri": uri, "diagnostics": [] }));
            effects
                .to_clients
                .push((client_id, publish_diagnostics(params)));
        }

        Ok(effects)
    }

    /// Handle `textDocument/didChange` from `client_id`
    pub fn did_change(&mut self, client_id: usize, params: &Value) -> Result<Effects> {
        let uri = text_document_uri(params)?;
        let version = params["textDocument"]["version"].as_i64();
        let changes = params["contentChanges"]
            .as_array()
            .context("missing contentChanges")?;

        let touched = self.tick();
        let encoding = self.encoding;
        let doc = self
            .docs
            .get_mut(uri)
            .context("change of a document the client doesn't have open")?;
        let client = doc
            .clients
            .get_mut(&client_id)
            .context("change of a document the client doesn't have open")?;
        for change in changes {
            apply_change(&mut client.text, change, encoding)?;
        }
        if let Some(version) = version {
            client.version = version;
        }
        client.touched = touched;
        client.matched_server_version =
            (client.text == doc.server.text).then_some(doc.server.version);

        Ok(Effects {
            to_server: self.resync(uri, client_id).into_iter().collect(),
            to_clients: Vec::new(),
        })
    }

    /// Handle `textDocument/didClose` from `client_id`
    pub fn did_close(&mut self, client_id: usize, uri: &str) -> Effects {
        let mut effects = Effects::default();
        let Some(doc) = self.docs.get_mut(uri) else {
            return effects;
        };
        if doc.clients.remove(&client_id).is_none() {
            return effects;
        }

        if doc.clients.is_empty() {
            self.closed_versions
                .insert(uri.to_owned(), doc.server.version);
            self.docs.remove(uri);
            effects.to_server.push(notification(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": uri } }),
            ));
        } else if doc
            .clients
            .values()
            .all(|client| client.text != doc.server.text)
        {
            let (&latest, _) = doc
                .clients
                .iter()
                .max_by_key(|(_, client)| client.touched)
                .unwrap();
            effects.to_server.extend(self.resync(uri, latest));
        } else if doc.server.synced_from == client_id {
            // The server text stays, it's now the text of a remaining client
            // with identical text. Not a switch to foreign text.
            let (&owner, _) = doc
                .clients
                .iter()
                .filter(|(_, client)| client.text == doc.server.text)
                .max_by_key(|(_, client)| client.touched)
                .unwrap();
            doc.server.synced_from = owner;
        }
        effects
    }

    /// Handle disconnect of `client_id`, closes all its documents
    pub fn close_client(&mut self, client_id: usize) -> Effects {
        let mut effects = Effects::default();
        for uri in self.files_of(client_id) {
            let closed = self.did_close(client_id, &uri);
            effects.to_server.extend(closed.to_server);
            effects.to_clients.extend(closed.to_clients);
        }
        effects
    }

    /// Notifications making the server see `client_id`'s text of every
    /// document it has open, to be sent right before its request `params`
    pub fn sync_for_request(&mut self, client_id: usize, params: &Value) -> Vec<Notification> {
        if let Ok(uri) = text_document_uri(params) {
            let touched = self.tick();
            if let Some(client) = self
                .docs
                .get_mut(uri)
                .and_then(|doc| doc.clients.get_mut(&client_id))
            {
                client.touched = touched;
            }
        }

        let stale = self
            .docs
            .iter()
            .filter(|(_, doc)| {
                doc.server.synced_from != client_id
                    && doc
                        .clients
                        .get(&client_id)
                        .is_some_and(|client| client.text != doc.server.text)
            })
            .map(|(uri, _)| uri.clone())
            .collect::<Vec<_>>();
        stale
            .iter()
            .filter_map(|uri| self.resync(uri, client_id))
            .collect()
    }

    /// Rewrite every versioned document identifier (`{uri, version}`) in a
    /// server message to `client_id`'s numbering
    ///
    /// Returns `false` when some version doesn't describe text the client
    /// still has, the message is then unsafe for the client and must not
    /// reach it. Unversioned identifiers (`version: null`) are left alone.
    #[must_use]
    pub fn translate_versions(&self, client_id: usize, value: &mut Value) -> bool {
        match value {
            Value::Object(object) => {
                if object.len() == 2 {
                    if let (Some(Value::String(uri)), Some(Value::Number(version))) =
                        (object.get("uri"), object.get("version"))
                    {
                        let translated = version.as_i64().and_then(|version| {
                            self.docs.get(uri.as_str())?.translate(client_id, version)
                        });
                        let Some(translated) = translated else {
                            return false;
                        };
                        object.insert("version".into(), translated.into());
                        return true;
                    }
                }
                object
                    .values_mut()
                    .all(|value| self.translate_versions(client_id, value))
            }
            Value::Array(array) => array
                .iter_mut()
                .all(|value| self.translate_versions(client_id, value)),
            _ => true,
        }
    }

    /// Handle `textDocument/publishDiagnostics` from the server
    ///
    /// Caches the params and returns what each of the `connected` clients
    /// should receive.
    pub fn publish_diagnostics(
        &mut self,
        params: Value,
        connected: &[usize],
    ) -> Vec<(usize, Notification)> {
        let deliveries = connected
            .iter()
            .filter_map(|&client_id| {
                let params = self.diagnostics_for(client_id, &params)?;
                Some((client_id, publish_diagnostics(params)))
            })
            .collect();

        if let Some(uri) = params["uri"].as_str() {
            let empty = params["diagnostics"]
                .as_array()
                .map_or(true, |diagnostics| diagnostics.is_empty());
            if empty {
                self.diagnostics.remove(uri);
            } else {
                self.diagnostics.insert(uri.to_owned(), params);
            }
        }

        deliveries
    }

    /// All cached diagnostics, for a newly connected client
    pub fn replay_diagnostics(&self) -> Vec<Notification> {
        self.diagnostics
            .values()
            .cloned()
            .map(publish_diagnostics)
            .collect()
    }

    /// State of every document `client_id` has open, to be taken when its
    /// request is sent to the server
    pub fn snapshot(&self, client_id: usize) -> Snapshot {
        Snapshot {
            docs: self
                .docs
                .iter()
                .filter_map(|(uri, doc)| {
                    let client = doc.clients.get(&client_id)?;
                    Some((uri.clone(), (doc.server.foreign_epoch, client.version)))
                })
                .collect(),
        }
    }

    /// A URI from `snapshot` whose server text switched to another client's
    /// text since the snapshot was taken
    pub fn foreign_switch_since<'s>(&self, snapshot: &'s Snapshot) -> Option<&'s str> {
        snapshot
            .docs
            .iter()
            .find(|(uri, (epoch, _))| {
                self.docs
                    .get(uri.as_str())
                    .is_some_and(|doc| doc.server.foreign_epoch != *epoch)
            })
            .map(|(uri, _)| uri.as_str())
    }

    /// Is an edit computed by the server for `client_id`'s request taken at
    /// `snapshot` still safe to apply to `uri`
    pub fn check_edit(
        &self,
        client_id: usize,
        snapshot: &Snapshot,
        uri: &str,
        read: ReadUri,
    ) -> Result<(), String> {
        let Some(client) = self
            .docs
            .get(uri)
            .and_then(|doc| doc.clients.get(&client_id).map(|client| (doc, client)))
        else {
            return self.check_not_open(client_id, uri, read);
        };
        let (doc, client) = client;
        let Some(&(epoch, version)) = snapshot.docs.get(uri) else {
            return Err(format!("{uri} wasn't open in the client when the request was sent"));
        };
        if doc.server.foreign_epoch != epoch {
            return Err(format!(
                "{uri} switched to another client's text since the request was sent"
            ));
        }
        if client.version != version {
            return Err(format!("{uri} was changed by the client since the request was sent"));
        }
        Ok(())
    }

    /// Is an edit of `uri`, which `client_id` doesn't have open, safe
    ///
    /// Without any client having it open the server works on disk text,
    /// which is also what the client edits. Otherwise the server works on
    /// another client's text, which must equal the disk text.
    pub fn check_not_open(&self, client_id: usize, uri: &str, read: ReadUri) -> Result<(), String> {
        debug_assert!(!self.has_open(client_id, uri));
        let Some(doc) = self.docs.get(uri) else {
            return Ok(());
        };
        let Some(disk) = read(uri) else {
            return Err(format!(
                "{uri} is open in another client and couldn't be read from disk"
            ));
        };
        if disk != doc.server.text {
            return Err(format!(
                "{uri} is open in another client with text that differs from disk"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(uri: &str, version: i64, text: &str) -> Value {
        json!({ "textDocument": { "uri": uri, "languageId": "haskell", "version": version, "text": text } })
    }

    fn change(uri: &str, version: i64, changes: Value) -> Value {
        json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": changes })
    }

    fn range(sl: u64, sc: u64, el: u64, ec: u64) -> Value {
        json!({ "start": { "line": sl, "character": sc }, "end": { "line": el, "character": ec } })
    }

    fn methods(notifs: &[Notification]) -> Vec<&str> {
        notifs.iter().map(|n| n.method.as_str()).collect()
    }

    fn diags(uri: &str, version: Option<i64>, n: usize) -> Value {
        let mut params = json!({
            "uri": uri,
            "diagnostics": vec![json!({ "message": "err", "range": range(0, 0, 0, 1) }); n],
        });
        if let Some(version) = version {
            params["version"] = version.into();
        }
        params
    }

    const U: &str = "file:///p/A.hs";

    #[test]
    fn incremental_utf16_surrogate_pairs() {
        // 😀 is two UTF-16 code units, four UTF-8 bytes
        let mut text = "a😀b\nx".to_owned();
        let edit = json!({ "range": range(0, 3, 0, 4), "text": "B" });
        apply_change(&mut text, &edit, PositionEncoding::Utf16).unwrap();
        assert_eq!(text, "a😀B\nx");

        let edit = json!({ "range": range(0, 1, 0, 3), "text": "" });
        apply_change(&mut text, &edit, PositionEncoding::Utf16).unwrap();
        assert_eq!(text, "aB\nx");

        let mut text = "a😀b".to_owned();
        let edit = json!({ "range": range(0, 1, 0, 2), "text": "!" });
        apply_change(&mut text, &edit, PositionEncoding::Utf32).unwrap();
        assert_eq!(text, "a!b");
    }

    #[test]
    fn incremental_multi_line() {
        let mut text = "one\r\ntwo\nthree\n".to_owned();
        let edit = json!({ "range": range(0, 1, 2, 2), "text": "X\nY" });
        apply_change(&mut text, &edit, PositionEncoding::Utf16).unwrap();
        assert_eq!(text, "oX\nYree\n");

        // past the end of a line and of the document
        let edit = json!({ "range": range(0, 99, 9, 0), "text": "!" });
        apply_change(&mut text, &edit, PositionEncoding::Utf16).unwrap();
        assert_eq!(text, "oX!");

        // full replacement
        apply_change(&mut text, &json!({ "text": "new" }), PositionEncoding::Utf16).unwrap();
        assert_eq!(text, "new");
    }

    #[test]
    fn incremental_utf8() {
        let mut text = "é😀z".to_owned();
        // é is 2 bytes, 😀 is 4 bytes
        let edit = json!({ "range": range(0, 2, 0, 6), "text": "-" });
        apply_change(&mut text, &edit, PositionEncoding::Utf8).unwrap();
        assert_eq!(text, "é-z");
    }

    #[test]
    fn position_encoding_from_capabilities() {
        use PositionEncoding::*;
        assert_eq!(PositionEncoding::from_server_capabilities(&json!({})), Utf16);
        let caps = json!({ "positionEncoding": "utf-8" });
        assert_eq!(PositionEncoding::from_server_capabilities(&caps), Utf8);
        let caps = json!({ "positionEncoding": "utf-32" });
        assert_eq!(PositionEncoding::from_server_capabilities(&caps), Utf32);
    }

    #[test]
    fn capability_rewrite() {
        let mut caps = json!({ "textDocumentSync": 2 });
        rewrite_text_document_sync(&mut caps);
        assert_eq!(caps, json!({ "textDocumentSync": 1 }));

        let mut caps = json!({ "textDocumentSync": {
            "openClose": true, "change": 2, "save": { "includeText": false }, "willSave": true,
        } });
        rewrite_text_document_sync(&mut caps);
        assert_eq!(
            caps,
            json!({ "textDocumentSync": {
                "openClose": true, "change": 1, "save": { "includeText": false }, "willSave": true,
            } })
        );

        let mut caps = json!({ "textDocumentSync": 0 });
        rewrite_text_document_sync(&mut caps);
        assert_eq!(caps, json!({ "textDocumentSync": 0 }));
    }

    #[test]
    fn second_opener_with_different_text_resyncs() {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        let e = docs.did_open(1, &open(U, 5, "a")).unwrap();
        assert_eq!(methods(&e.to_server), ["textDocument/didOpen"]);
        assert_eq!(e.to_server[0].params["textDocument"]["version"], 1);

        let e = docs.did_open(2, &open(U, 1, "b")).unwrap();
        assert_eq!(methods(&e.to_server), ["textDocument/didChange"]);
        assert_eq!(e.to_server[0].params["textDocument"]["version"], 2);
        assert_eq!(e.to_server[0].params["contentChanges"][0]["text"], "b");

        // client 1 asks for something, server goes back to client 1's text
        let n = docs.sync_for_request(1, &json!({ "textDocument": { "uri": U } }));
        assert_eq!(methods(&n), ["textDocument/didChange"]);
        assert_eq!(n[0].params["textDocument"]["version"], 3);
        assert_eq!(n[0].params["contentChanges"][0]["text"], "a");
        assert!(docs.sync_for_request(1, &json!({})).is_empty());
    }

    #[test]
    fn identical_text_sends_nothing() {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 1, "a")).unwrap();
        let e = docs.did_open(2, &open(U, 7, "a")).unwrap();
        assert!(e.to_server.is_empty());
        assert!(docs.sync_for_request(2, &json!({})).is_empty());

        // a raw change is never forwarded, only a full resync when needed
        let edit = json!([{ "range": range(0, 1, 0, 1), "text": "!" }]);
        let e = docs.did_change(2, &change(U, 8, edit)).unwrap();
        assert_eq!(methods(&e.to_server), ["textDocument/didChange"]);
        assert_eq!(e.to_server[0].params["contentChanges"], json!([{ "text": "a!" }]));
    }

    #[test]
    fn owner_closes_server_resyncs_to_remaining() {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 1, "a")).unwrap();
        docs.did_open(2, &open(U, 1, "b")).unwrap();
        docs.did_open(3, &open(U, 1, "c")).unwrap();
        // client 2 interacted most recently among the remaining ones
        docs.did_change(2, &change(U, 2, json!([{ "text": "bb" }])))
            .unwrap();
        docs.sync_for_request(1, &json!({}));

        let e = docs.did_close(1, U);
        assert_eq!(methods(&e.to_server), ["textDocument/didChange"]);
        assert_eq!(e.to_server[0].params["contentChanges"][0]["text"], "bb");

        // server text matches a remaining client, nothing to do
        let e = docs.did_close(3, U);
        assert!(e.to_server.is_empty());
    }

    #[test]
    fn last_close_sends_did_close() {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 1, "a")).unwrap();
        docs.did_open(2, &open(U, 1, "a")).unwrap();
        assert!(docs.close_client(1).to_server.is_empty());
        let e = docs.close_client(2);
        assert_eq!(methods(&e.to_server), ["textDocument/didClose"]);
        assert!(!docs.has_open(2, U));

        // reopening continues the server numbering
        let e = docs.did_open(1, &open(U, 1, "a")).unwrap();
        assert_eq!(e.to_server[0].params["textDocument"]["version"], 2);
    }

    fn edit_version(version: i64) -> Value {
        json!({ "documentChanges": [
            { "textDocument": { "uri": U, "version": version }, "edits": [] },
        ] })
    }

    fn translated(docs: &Docs, client_id: usize, version: i64) -> Option<Value> {
        let mut msg = edit_version(version);
        docs.translate_versions(client_id, &mut msg)
            .then(|| msg["documentChanges"][0]["textDocument"]["version"].clone())
    }

    #[test]
    fn version_mapping_current_match() {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 10, "a")).unwrap();
        docs.did_open(2, &open(U, 20, "b")).unwrap();
        // server is at version 2 with client 2's text
        assert_eq!(translated(&docs, 2, 2), Some(json!(20)));
        // never had client 1's text at version 2, nor client 2's at version 1
        assert_eq!(translated(&docs, 1, 2), None);
        assert_eq!(translated(&docs, 2, 1), None);

        // another client moving the server to this client's text matches too:
        // client 3 opening "a" makes the server (version 3) match client 1
        docs.did_open(3, &open(U, 30, "a")).unwrap();
        assert_eq!(translated(&docs, 1, 3), Some(json!(10)));
        assert_eq!(translated(&docs, 3, 3), Some(json!(30)));

        // unversioned identifiers are left alone
        let mut msg = json!({ "textDocument": { "uri": U, "version": null } });
        assert!(docs.translate_versions(1, &mut msg));
        assert_eq!(msg["textDocument"]["version"], Value::Null);
    }

    #[test]
    fn version_mapping_older_match_after_another_client_moved_server() {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 10, "a")).unwrap();
        // client 2 moves the server to version 2, client 1 didn't change
        docs.did_open(2, &open(U, 20, "b")).unwrap();
        assert_eq!(translated(&docs, 1, 1), Some(json!(10)));

        // client 1 makes the server see its text again, then client 2 moves it on
        docs.sync_for_request(1, &json!({}));
        docs.sync_for_request(2, &json!({}));
        assert_eq!(translated(&docs, 1, 3), Some(json!(10)));
        assert_eq!(translated(&docs, 1, 1), None);
    }

    #[test]
    fn version_mapping_client_changed_since_is_unsafe() {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 10, "a")).unwrap();
        docs.did_open(2, &open(U, 20, "a")).unwrap();
        assert_eq!(translated(&docs, 2, 1), Some(json!(20)));

        // client 2 changes, the server follows, client 1's text is older
        docs.did_change(2, &change(U, 21, json!([{ "text": "ab" }])))
            .unwrap();
        assert_eq!(translated(&docs, 2, 1), None);
        assert_eq!(translated(&docs, 2, 2), Some(json!(21)));
        assert_eq!(translated(&docs, 1, 1), Some(json!(10)));

        // client 1 changing to the server's current text matches it
        docs.did_change(1, &change(U, 11, json!([{ "text": "ab" }])))
            .unwrap();
        assert_eq!(translated(&docs, 1, 1), None);
        assert_eq!(translated(&docs, 1, 2), Some(json!(11)));
    }

    #[test]
    fn diagnostics_filter() {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 10, "a")).unwrap();
        docs.did_open(2, &open(U, 20, "b")).unwrap();
        // server is at version 2 with client 2's text, client 3 has nothing open

        let out = docs.publish_diagnostics(diags(U, Some(2), 1), &[1, 2, 3]);
        let out = out
            .into_iter()
            .map(|(c, n)| (c, n.params["version"].clone()))
            .collect::<Vec<_>>();
        assert_eq!(out, [(2, json!(20)), (3, json!(2))]);

        // unversioned, only the client with the server's text gets them
        let out = docs.publish_diagnostics(diags(U, None, 1), &[1, 2]);
        let out = out
            .into_iter()
            .map(|(c, n)| (c, n.params["version"].clone()))
            .collect::<Vec<_>>();
        assert_eq!(out, [(2, json!(20))]);
    }

    #[test]
    fn diagnostics_cache_replay_prune_and_clear() {
        const V: &str = "file:///p/B.hs";
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 1, "a")).unwrap();
        docs.publish_diagnostics(diags(U, Some(1), 1), &[1]);
        docs.publish_diagnostics(diags(V, None, 2), &[1]);

        // connect-time replay contains every cached URI
        let mut uris = docs
            .replay_diagnostics()
            .iter()
            .map(|n| n.params["uri"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        uris.sort();
        assert_eq!(uris, [U, V]);

        // an empty publish prunes the entry
        docs.publish_diagnostics(diags(V, None, 0), &[1]);
        assert_eq!(docs.replay_diagnostics().len(), 1);

        // a mismatched open clears what was replayed
        let e = docs.did_open(2, &open(U, 1, "other")).unwrap();
        assert_eq!(e.to_clients.len(), 1);
        assert_eq!(e.to_clients[0].0, 2);
        assert_eq!(
            e.to_clients[0].1.params,
            json!({ "uri": U, "diagnostics": [] })
        );

        // an identical open gets the cached set in its own numbering
        let mut docs2 = Docs::new(PositionEncoding::Utf16);
        docs2.did_open(1, &open(U, 1, "a")).unwrap();
        docs2.publish_diagnostics(diags(U, Some(1), 1), &[1]);
        let e = docs2.did_open(2, &open(U, 42, "a")).unwrap();
        assert_eq!(e.to_clients[0].1.params["version"], 42);
        assert_eq!(e.to_clients[0].1.params["diagnostics"].as_array().unwrap().len(), 1);
    }

    fn epoch(docs: &Docs) -> u64 {
        docs.docs[U].server.foreign_epoch
    }

    fn full(text: &str) -> Value {
        json!([{ "text": text }])
    }

    #[test]
    fn foreign_epoch_bumps_only_on_foreign_switch() {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 1, "a")).unwrap();
        let e0 = epoch(&docs);

        // own changes while synced from the client
        docs.did_change(1, &change(U, 2, full("ab"))).unwrap();
        assert_eq!(epoch(&docs), e0);
        // another client opening identical text doesn't switch anything
        docs.did_open(2, &open(U, 1, "ab")).unwrap();
        assert_eq!(epoch(&docs), e0);
        // a no-op request resync
        docs.sync_for_request(1, &json!({}));
        assert_eq!(epoch(&docs), e0);

        // another client's change switches the server text
        docs.did_change(2, &change(U, 2, full("b"))).unwrap();
        let e1 = epoch(&docs);
        assert_ne!(e1, e0);
        docs.did_change(2, &change(U, 3, full("bb"))).unwrap();
        assert_eq!(epoch(&docs), e1);

        // a request of the other client switches back
        docs.sync_for_request(1, &json!({}));
        let e2 = epoch(&docs);
        assert_ne!(e2, e1);

        // closing the owner resyncs to the remaining client
        docs.close_client(1);
        assert_ne!(epoch(&docs), e2);
    }

    #[test]
    fn foreign_epoch_closing_owner_with_identical_text_is_no_switch() {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 1, "a")).unwrap();
        docs.did_open(2, &open(U, 1, "a")).unwrap();
        let e0 = epoch(&docs);
        docs.close_client(1);
        assert_eq!(epoch(&docs), e0);
        // the remaining client is the owner now, its changes are its own
        docs.did_change(2, &change(U, 2, full("b"))).unwrap();
        assert_eq!(epoch(&docs), e0);

        // reopening after everyone closed is a new epoch
        docs.close_client(2);
        docs.did_open(3, &open(U, 1, "a")).unwrap();
        assert_ne!(epoch(&docs), e0);
    }

    #[test]
    fn snapshot_comparisons() {
        let no_disk = |_: &str| -> Option<String> { panic!("no disk access expected") };
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 1, "a")).unwrap();
        docs.did_open(2, &open(U, 1, "a")).unwrap();
        let snapshot = docs.snapshot(1);
        assert_eq!(docs.foreign_switch_since(&snapshot), None);
        assert_eq!(docs.check_edit(1, &snapshot, U, &no_disk), Ok(()));

        // own edit: no foreign switch but the edit is refused
        docs.did_change(1, &change(U, 2, full("a1"))).unwrap();
        assert_eq!(docs.foreign_switch_since(&snapshot), None);
        assert!(docs
            .check_edit(1, &snapshot, U, &no_disk)
            .unwrap_err()
            .contains("changed by the client"));

        // other client's edit: foreign switch
        let snapshot = docs.snapshot(1);
        docs.did_change(2, &change(U, 2, full("b"))).unwrap();
        assert_eq!(docs.foreign_switch_since(&snapshot), Some(U));
        assert!(docs
            .check_edit(1, &snapshot, U, &no_disk)
            .unwrap_err()
            .contains("another client's text"));

        // opened after the request was sent
        let snapshot = docs.snapshot(3);
        docs.did_open(3, &open(U, 1, "c")).unwrap();
        assert!(docs
            .check_edit(3, &snapshot, U, &no_disk)
            .unwrap_err()
            .contains("wasn't open"));
    }

    #[test]
    fn disk_check_outcomes() {
        const O: &str = "file:///p/Other.hs";
        let disk = |text: &'static str| move |uri: &str| (uri == O).then(|| text.to_owned());
        let unreadable = |_: &str| None;
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(1, &open(U, 1, "a")).unwrap();
        let snapshot = docs.snapshot(1);

        // nobody has it open, the server uses disk text
        assert_eq!(docs.check_edit(1, &snapshot, O, &unreadable), Ok(()));

        // another client has it open
        docs.did_open(2, &open(O, 1, "on disk")).unwrap();
        assert_eq!(docs.check_edit(1, &snapshot, O, &disk("on disk")), Ok(()));
        assert!(docs
            .check_edit(1, &snapshot, O, &disk("different"))
            .unwrap_err()
            .contains("differs from disk"));
        assert!(docs
            .check_edit(1, &snapshot, O, &unreadable)
            .unwrap_err()
            .contains("couldn't be read"));

        // a third client with unsaved text moves the server away from disk
        docs.did_open(3, &open(O, 1, "unsaved")).unwrap();
        assert!(docs.check_not_open(1, O, &disk("on disk")).is_err());
        // back on the disk text
        docs.sync_for_request(2, &json!({}));
        assert_eq!(docs.check_not_open(1, O, &disk("on disk")), Ok(()));
    }
}
