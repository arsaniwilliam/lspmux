use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::io::ErrorKind;
use std::ops::Deref;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::error::SendError;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::MissedTickBehavior;
use tokio::{select, task};
use tracing::{debug, error, field, info, instrument, trace, warn, Instrument};

use crate::capabilities::{self, refresh_capability, Registrations, REFRESH_REQUESTS};
use crate::client::Client;
use crate::config::Config;
use crate::docs::{self, Docs, PositionEncoding, ReadUri, Snapshot};
use crate::lsp::ext::Tag;
use crate::lsp::jsonrpc::{
    self, Message, Notification, Request, RequestId, ResponseError, ResponseSuccess, Version,
};
use crate::lsp::transport::{LspReader, LspWriter};
use crate::lsp::{self, ext};
use crate::progress::Progress;
use crate::settings;

/// Specifies server configuration
///
/// If another server with the same configuration is requested we can reuse it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InstanceKey {
    pub server: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub workspace_root: String,
}

/// Language server instance
///
/// # Lock order
///
/// When more than one of the mutexes is held at the same time they must be
/// acquired in this order (any subset, skipping is fine):
///
/// `clients` → `dynamic_capabilities` → `docs` → `progress` →
/// `request_snapshots` → `execute_commands`
///
/// `docs` is held across sends into the server channel so client document
/// resyncs and the request they're for reach the server without other
/// clients' messages in between.
pub struct Instance {
    key: InstanceKey,

    /// Language server child process id
    pid: u32,

    /// Server's response to `initialize` request, rewritten for clients
    init_result: lsp::InitializeResult,

    /// Response to `workspace/workspaceFolders` server requests: the folders
    /// the instance was initialized with
    workspace_folders: Value,

    /// Handle for sending messages to the language server instance
    server: mpsc::Sender<Message>,

    /// Associated clients
    clients: Mutex<HashMap<usize, Client>>,

    /// Dynamic capabilities registered by the server and which clients
    /// received them
    dynamic_capabilities: Mutex<Registrations>,

    /// Client and server copies of open documents and cached diagnostics
    docs: Mutex<Docs>,

    /// Active server progress and when the server was last active
    progress: Mutex<Progress>,

    /// How long the server must be quiet before owed diagnostics are
    /// answered by lspmux
    settle: Duration,

    /// Settings object lspmux answers `workspace/configuration` with instead
    /// of the clients, from `server_settings`
    settings: Option<Value>,

    /// Document state of each client request the server hasn't responded to
    /// yet, at the time it was sent, keyed by the tagged request ID
    request_snapshots: Mutex<HashMap<String, Snapshot>>,

    /// Client `workspace/executeCommand` requests, used to route server
    /// requests back to the triggering client
    execute_commands: Mutex<ExecuteCommandTracker>,

    /// Wakes up `wait_task` and asks it to send SIGKILL to the instance.
    close: Notify,

    /// Last time a message was sent to this instance
    ///
    /// Uses UTC unix timestamp ([utc_now] function)
    last_used: AtomicI64,
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Make sure we're not leaking anything
        debug!("instance dropped");
    }
}

/// A client `workspace/executeCommand` request the server hasn't responded to
struct InFlightCommand {
    /// Tagged request ID
    key: String,
    client_id: usize,
    /// The client disconnected, the entry stays until the server responds
    orphaned: bool,
    /// The client's documents when the command was sent
    snapshot: Snapshot,
}

/// Book-keeping of client `workspace/executeCommand` requests
#[derive(Default)]
struct ExecuteCommandTracker {
    /// In arrival order
    in_flight: Vec<InFlightCommand>,
}

impl ExecuteCommandTracker {
    /// Distinct clients with commands in flight
    fn owners(&self) -> Vec<Owner> {
        let mut owners = Vec::<Owner>::new();
        for command in &self.in_flight {
            if let Some(owner) = owners.iter_mut().find(|o| o.client_id == command.client_id) {
                owner.orphaned |= command.orphaned;
            } else {
                owners.push(Owner {
                    client_id: command.client_id,
                    orphaned: command.orphaned,
                });
            }
        }
        owners
    }

    /// Document snapshots of `client_id`'s commands in flight
    fn snapshots_of(&self, client_id: usize) -> impl Iterator<Item = &Snapshot> {
        self.in_flight
            .iter()
            .filter(move |command| command.client_id == client_id)
            .map(|command| &command.snapshot)
    }
}

/// Client with a `workspace/executeCommand` request in flight
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Owner {
    client_id: usize,
    orphaned: bool,
}

/// Choose the client a server request caused by `workspace/executeCommand`
/// belongs to, or the reason why it can't be determined (fail closed)
///
/// A server request doesn't say which command caused it, so it is attributed
/// only when exactly one connected client has commands in flight. Which
/// documents a client has open says nothing about ownership: a command may
/// edit a file only another client has open.
fn select_request_owner(owners: &[Owner]) -> Result<usize, &'static str> {
    match owners {
        [] => Err("no workspace/executeCommand in flight"),
        [owner] if owner.orphaned => Err("the client which triggered the request disconnected"),
        [owner] => Ok(owner.client_id),
        _ if owners.iter().any(|owner| owner.orphaned) => {
            Err("a disconnected client has a workspace/executeCommand in flight")
        }
        _ => Err("several clients have a workspace/executeCommand in flight"),
    }
}

/// URIs of all documents touched by `WorkspaceEdit`s anywhere in `value`
///
/// Every object with a `changes` map or a `documentChanges` list counts as a
/// `WorkspaceEdit`, so this covers `workspace/applyEdit` params as well as
/// results like code actions or renames.
fn workspace_edit_uris(value: &Value) -> Vec<String> {
    let mut uris = Vec::new();
    collect_workspace_edit_uris(value, &mut uris);
    uris
}

fn collect_workspace_edit_uris(value: &Value, uris: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            let changes = object.get("changes").and_then(Value::as_object);
            let document_changes = object.get("documentChanges").and_then(Value::as_array);
            if changes.is_none() && document_changes.is_none() {
                for value in object.values() {
                    collect_workspace_edit_uris(value, uris);
                }
                return;
            }
            uris.extend(
                changes
                    .into_iter()
                    .flat_map(|changes| changes.keys().cloned()),
            );
            for change in document_changes.into_iter().flatten() {
                let fields = [
                    &change["textDocument"]["uri"],
                    &change["uri"],
                    &change["oldUri"],
                    &change["newUri"],
                ];
                uris.extend(
                    fields
                        .into_iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                );
            }
        }
        Value::Array(array) => {
            for value in array {
                collect_workspace_edit_uris(value, uris);
            }
        }
        _ => {}
    }
}

/// Current disk text of a `file://` URI
fn read_file_uri(uri: &str) -> Option<String> {
    let path = crate::client::parse_root_uri(uri).ok()?;
    std::fs::read_to_string(path).ok()
}

/// Is an edit of `uris`, caused by `client_id`'s commands in flight taken at
/// `snapshots`, safe to apply in the client
fn check_apply_edit<'s>(
    docs: &Docs,
    client_id: usize,
    snapshots: impl IntoIterator<Item = &'s Snapshot>,
    uris: &[String],
    read: ReadUri,
) -> Result<(), String> {
    // The command which caused the edit is unknown with several of them in
    // flight, the edit must be safe for all of them.
    let mut any = false;
    for snapshot in snapshots {
        any = true;
        for uri in uris {
            docs.check_edit(client_id, snapshot, uri, read)?;
        }
    }
    if !any {
        return Err("no workspace/executeCommand in flight".into());
    }
    Ok(())
}

/// String key identifying a (tagged) request ID
fn request_key(id: &RequestId) -> String {
    match id {
        RequestId::Number(number) => number.to_string(),
        RequestId::String(string) => string.clone(),
    }
}

/// Successful response to `workspace/applyEdit` refusing the edit
fn apply_edit_failure(id: RequestId, reason: &str) -> ResponseSuccess {
    ResponseSuccess {
        jsonrpc: Version,
        result: json!({
            "applied": false,
            "failureReason": format!("lspmux: {reason}"),
        }),
        id,
    }
}

/// JSON-RPC `ContentModified` error code
const CONTENT_MODIFIED: i64 = -32801;

fn content_modified(id: RequestId, message: &str) -> ResponseError {
    ResponseError {
        jsonrpc: Version,
        error: jsonrpc::Error {
            code: CONTENT_MODIFIED,
            message: message.into(),
            data: None,
        },
        id,
    }
}

/// Message of the `ContentModified` error replacing results which may be
/// based on another client's text
const FOREIGN_TEXT: &str = "lspmux: result was computed against another client's document text";

/// Server response to `client_id`'s request sent at `snapshot` in the client's
/// document version numbering, or a `ContentModified` error when it may be
/// based on text the client doesn't have
fn response_for_client(
    docs: &Docs,
    client_id: usize,
    snapshot: &Snapshot,
    mut res: ResponseSuccess,
    read: ReadUri,
) -> Message {
    if let Some(uri) = docs.foreign_switch_since(snapshot) {
        warn!(
            ?client_id,
            uri, "server switched to another client's text during the request"
        );
        return content_modified(res.id, FOREIGN_TEXT).into();
    }
    for uri in workspace_edit_uris(&res.result) {
        if docs.has_open(client_id, &uri) {
            continue;
        }
        if let Err(reason) = docs.check_not_open(client_id, &uri, read) {
            warn!(
                ?client_id,
                reason, "server response edits a document unsafe for the client"
            );
            return content_modified(res.id, FOREIGN_TEXT).into();
        }
    }
    if docs.translate_versions(client_id, &mut res.result) {
        res.into()
    } else {
        warn!(?client_id, "server response unsafe for the client");
        content_modified(
            res.id,
            "lspmux: result refers to a document version this client no longer has",
        )
        .into()
    }
}

/// Server request for `client_id` in the client's document version numbering
///
/// When it refers to text the client no longer has the request must not reach
/// the client, the error is the response for the server instead.
fn request_for_client(docs: &Docs, client_id: usize, mut req: Request) -> Result<Request, Message> {
    if docs.translate_versions(client_id, &mut req.params) {
        return Ok(req);
    }
    Err(if req.method == "workspace/applyEdit" {
        let reason = format!("edit targets a document version client {client_id} no longer has");
        apply_edit_failure(req.id, &reason).into()
    } else {
        content_modified(
            req.id,
            "lspmux: request refers to a document version this client no longer has",
        )
        .into()
    })
}

/// Params of a server message for every client it's safe for, in each
/// client's document version numbering
fn translated_for_each(
    docs: &Docs,
    clients: &HashMap<usize, Client>,
    params: &Value,
) -> Vec<(usize, Value)> {
    clients
        .keys()
        .filter_map(|&client_id| {
            let mut params = params.clone();
            docs.translate_versions(client_id, &mut params)
                .then_some((client_id, params))
        })
        .collect()
}

// Current unix timestamp with second precission
fn utc_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Diagnostics owed to clients, if the server settled
fn owed_when_settled(
    docs: &mut Docs,
    progress: &Progress,
    now: Instant,
    settle: Duration,
) -> Vec<(usize, Notification)> {
    if !docs.any_owed() || !progress.settled(now, settle) {
        return Vec::new();
    }
    docs.take_owed_diagnostics()
}

impl Instance {
    /// Mark the instance as used
    pub fn keep_alive(&self) {
        self.last_used.store(utc_now(), Ordering::Relaxed);
    }

    /// How many seconds is the instance idle for
    pub fn idle(&self) -> i64 {
        i64::max(0, utc_now() - self.last_used.load(Ordering::Relaxed))
    }

    pub fn initialize_result(&self) -> lsp::InitializeResult {
        self.init_result.clone()
    }

    /// Does lspmux answer the server's configuration requests itself
    pub fn owns_settings(&self) -> bool {
        self.settings.is_some()
    }

    /// Add client to the instance so it can receive traffic from it
    ///
    /// It replays the registered dynamic capabilities the client supports,
    /// all cached diagnostics and active progress to it.
    pub async fn add_client(&self, client: Client) {
        let mut clients = self.clients.lock().await;

        let registrations = self
            .dynamic_capabilities
            .lock()
            .await
            .add_client(client.id(), client.record().clone());
        if !registrations.is_empty() {
            // We will drop the client response and we need to make sure the
            // request ID is unique.
            let id = RequestId::String("replay:registerCapabilities".into()).tag(Tag::Drop);
            let params = lsp::RegistrationParams { registrations };
            let req = Request {
                id,
                method: "client/registerCapability".into(),
                params: serde_json::to_value(params).unwrap(),
                jsonrpc: Version,
            };
            debug!(?req, "replaying server request");
            let _ = client.send_message(req.into()).await;
        }

        let diagnostics = self.docs.lock().await.replay_diagnostics();
        debug!(count = diagnostics.len(), "replaying cached diagnostics");
        for notif in diagnostics {
            let _ = client.send_message(notif.into()).await;
        }

        let work_done_progress = client.record().declares("window.workDoneProgress");
        let progress = self.progress.lock().await.attach(work_done_progress);
        debug!(count = progress.len(), "replaying progress");
        for message in progress {
            let _ = client.send_message(message).await;
        }

        if clients.insert(client.id(), client).is_some() {
            unreachable!("BUG: added two clients with the same ID");
        }
    }

    /// Send cleanup messages and remove remove client for client map
    pub async fn cleanup_client(&self, client: Client) -> Result<()> {
        debug!("cleaning up client");

        let mut clients = self.clients.lock().await;
        let removed = clients.remove(&client.id());
        if clients.is_empty() {
            // Idle time counts from the last disconnect, not the last message, so
            // `instance_timeout` is a grace period after the last client leaves.
            self.keep_alive();
        }

        self.dynamic_capabilities
            .lock()
            .await
            .remove_client(client.id());

        {
            let mut docs = self.docs.lock().await;
            for notif in docs.close_client(client.id()).to_server {
                debug!(?notif, "client disconnected, syncing server documents");
                let _ = self.send_sync(notif).await;
            }

            // Commands keep their entry until the server responds so a
            // request they cause can't be misrouted to another client.
            let mut execute_commands = self.execute_commands.lock().await;
            for command in &mut execute_commands.in_flight {
                if command.client_id == client.id() {
                    command.orphaned = true;
                }
            }
        }

        if removed.is_none() {
            // TODO This happens for example when the language server died while
            // client was still connected, and the client cleanup is attempted
            // with the instance being gone already. We should try notifying
            // these clients immediately and handling the cleanup separately.
            bail!("client was not connected");
        }

        Ok(())
    }

    /// Forward a client request (with an already tagged ID) to the server
    ///
    /// First makes the server see the client's text of all documents it has
    /// open, the resync, the snapshot of the client's documents and the
    /// request all happen under one `docs` lock.
    pub async fn forward_request(
        &self,
        client_id: usize,
        req: Request,
    ) -> Result<(), SendError<Message>> {
        let mut docs = self.docs.lock().await;
        for notif in docs.sync_for_request(client_id, &req.params) {
            debug!(?notif, "syncing server document to requesting client");
            self.send_sync(notif).await?;
        }
        // Recorded before sending so the response can't overtake it.
        let key = request_key(&req.id);
        let snapshot = docs.snapshot(client_id);
        if req.method == "workspace/executeCommand" {
            // Remember who triggered the command so server requests it causes
            // can be routed back and checked against the client's documents.
            self.execute_commands
                .lock()
                .await
                .in_flight
                .push(InFlightCommand {
                    key: key.clone(),
                    client_id,
                    orphaned: false,
                    snapshot: snapshot.clone(),
                });
        }
        self.request_snapshots.lock().await.insert(key, snapshot);
        self.send_message(req.into()).await
    }

    /// Remove the snapshot of a client request the server responded to
    async fn take_request_snapshot(&self, id: &RequestId) -> Snapshot {
        self.request_snapshots
            .lock()
            .await
            .remove(&request_key(id))
            .unwrap_or_default()
    }

    /// Forward a client notification to the server, ordered with document sync
    pub async fn forward_notification(
        &self,
        notif: Notification,
    ) -> Result<(), SendError<Message>> {
        let _docs = self.docs.lock().await;
        self.send_message(notif.into()).await
    }

    /// Should `workspace/didChangeWatchedFiles` from `client_id` reach the
    /// server
    pub async fn forwards_watched_files_from(&self, client_id: usize) -> bool {
        self.dynamic_capabilities
            .lock()
            .await
            .forwards_watched_files_from(client_id)
    }

    /// Forget a tracked `workspace/executeCommand` request after the server responded
    async fn finish_execute_command(&self, id: &RequestId) {
        let mut execute_commands = self.execute_commands.lock().await;
        if execute_commands.in_flight.is_empty() {
            return;
        }
        let key = request_key(id);
        execute_commands
            .in_flight
            .retain(|command| command.key != key);
    }

    /// Send a message to the language server channel
    pub async fn send_message(&self, message: Message) -> Result<(), SendError<Message>> {
        self.server.send(message).await
    }

    /// Send a document notification to the server
    ///
    /// Opens, changes and saves make the server recheck, possibly other
    /// documents too, which delays settling.
    async fn send_sync(&self, notif: Notification) -> Result<(), SendError<Message>> {
        if matches!(
            notif.method.as_str(),
            "textDocument/didOpen" | "textDocument/didChange" | "textDocument/didSave"
        ) {
            self.progress.lock().await.touch(Instant::now());
        }
        self.send_message(notif.into()).await
    }

    /// Send document sync notifications to the server
    async fn send_effects(&self, effects: docs::Effects) -> Vec<Notification> {
        for notif in effects.to_server {
            debug!(?notif, "syncing server document");
            let _ = self.send_sync(notif).await;
        }
        effects
            .to_clients
            .into_iter()
            .map(|(_, notif)| notif)
            .collect()
    }

    /// Handle `textDocument/didOpen` client notification
    ///
    /// Returns diagnostics the client should receive.
    pub async fn open_file(&self, client_id: usize, params: Value) -> Result<Vec<Notification>> {
        let mut docs = self.docs.lock().await;
        let effects = docs.did_open(client_id, &params)?;
        Ok(self.send_effects(effects).await)
    }

    /// Handle `textDocument/didChange` client notification
    pub async fn change_file(&self, client_id: usize, params: Value) -> Result<()> {
        let mut docs = self.docs.lock().await;
        let effects = docs.did_change(client_id, &params)?;
        self.send_effects(effects).await;
        Ok(())
    }

    /// Handle `textDocument/didSave` client notification, it's forwarded
    pub async fn save_file(
        &self,
        client_id: usize,
        notif: Notification,
    ) -> Result<(), SendError<Message>> {
        let mut docs = self.docs.lock().await;
        if let Some(uri) = notif.params["textDocument"]["uri"].as_str() {
            docs.did_save(client_id, uri);
        }
        self.send_sync(notif).await
    }

    /// Handle `textDocument/didClose` client notification
    pub async fn close_file(&self, client_id: usize, params: Value) -> Result<()> {
        let uri = params["textDocument"]["uri"]
            .as_str()
            .context("missing textDocument.uri")?;
        let mut docs = self.docs.lock().await;
        let effects = docs.did_close(client_id, uri);
        self.send_effects(effects).await;
        Ok(())
    }

    /// Send owed diagnostics if the server settled
    async fn answer_owed_diagnostics(&self) {
        let clients = self.clients.lock().await;
        let deliveries = {
            let mut docs = self.docs.lock().await;
            let progress = self.progress.lock().await;
            owed_when_settled(&mut docs, &progress, Instant::now(), self.settle)
        };
        for (client_id, notif) in deliveries {
            if let Some(client) = clients.get(&client_id) {
                debug!(
                    ?client_id,
                    ?notif,
                    "server settled, answering owed diagnostics"
                );
                let _ = client.send_message(notif.into()).await;
            }
        }
    }

    async fn get_status(&self) -> ext::Instance {
        let clients = {
            let clients = self.clients.lock().await;
            let docs = self.docs.lock().await;
            clients
                .values()
                .map(|client| ext::Client {
                    id: client.id(),
                    name: client.record().name().map(str::to_owned),
                    files: docs.files_of(client.id()),
                })
                .collect()
        };

        let registered_dyn_capabilities = self.dynamic_capabilities.lock().await.methods();

        ext::Instance {
            pid: self.pid,
            server: self.key.server.clone(),
            args: self.key.args.clone(),
            env: self.key.env.clone(),
            workspace_root: self.key.workspace_root.clone(),
            last_used: self.last_used.load(Ordering::Relaxed),
            clients,
            registered_dyn_capabilities,
        }
    }
}

/// What instances take from the configuration
struct InstanceOptions {
    settle: Duration,
    server_settings: BTreeMap<String, String>,
}

pub struct InstanceMap {
    instances: HashMap<InstanceKey, Arc<Instance>>,
    options: Arc<InstanceOptions>,
}

impl InstanceMap {
    pub fn new(config: &Config) -> Arc<Mutex<Self>> {
        let instance_map = Arc::new(Mutex::new(InstanceMap {
            instances: HashMap::new(),
            options: Arc::new(InstanceOptions {
                settle: Duration::from_millis(config.settle_ms),
                server_settings: config.server_settings.clone(),
            }),
        }));
        task::spawn(gc_task(
            instance_map.clone(),
            config.gc_interval,
            config.instance_timeout,
        ));
        instance_map
    }

    /// Finds an instance with the longest path such as
    /// `cwd.starts_with(workspace_root)` is true
    pub fn get_by_cwd(&self, cwd: &str) -> Option<&Instance> {
        self.instances
            .iter()
            .filter(|(key, _)| Path::new(cwd).starts_with(&key.workspace_root))
            .max_by_key(|(key, _)| key.workspace_root.len())
            .map(|(_, inst)| inst.deref())
    }

    pub async fn get_status(&self) -> ext::StatusResponse {
        let mut instances = Vec::with_capacity(self.instances.len());
        for instance in self.instances.values() {
            instances.push(instance.get_status().await);
        }
        ext::StatusResponse { instances }
    }
}

/// Periodically check for for idle language server instances
#[instrument("garbage collector", skip_all)]
async fn gc_task(
    instance_map: Arc<Mutex<InstanceMap>>,
    gc_interval: u32,
    instance_timeout: Option<u32>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(gc_interval.into()));
    loop {
        interval.tick().await;

        for (key, instance) in &instance_map.lock().await.instances {
            let clients = instance.clients.lock().await;

            let idle = instance.idle();
            debug!(path = ?key.workspace_root, idle, clients = clients.len(), "check instance");

            if let Some(instance_timeout) = instance_timeout {
                // Close timed out instance
                if idle > i64::from(instance_timeout) && clients.is_empty() {
                    info!(pid = instance.pid, path = ?key.workspace_root, idle, "instance timed out");
                    instance.close.notify_one();
                }
            }
        }
    }
}

/// Find existing or spawn a new language server instance
///
/// The instance is looked up based on `instance_key`. If an existing one is
/// found then it's returned and `init_req_params` are discarded. If it's
/// not found a new instance is spawned and initialized using the provided
/// `init_req_params`, this insance is then inserted into the map and returned.
pub async fn get_or_spawn(
    map: Arc<Mutex<InstanceMap>>,
    key: InstanceKey,
    init_req_params: lsp::InitializeParams,
) -> Result<Arc<Instance>> {
    // We have locked a clone of an Arc of the map, we can assume noone else
    // tries to spawn the same instance again. But we have to make sure `spawn`
    // doesn't try to lock its copy as well. This is a bit unfortunate code
    // organization but we want to have spawn in a separate tracing context and
    // we want to include `wait_task` in it as well in it as well
    let map_ref = map.clone();
    let mut locked = map_ref.lock().await;
    let options = locked.options.clone();
    match locked.instances.entry(key.clone()) {
        Entry::Occupied(e) => {
            info!("reusing language server instance");
            Ok(e.get().clone())
        }
        Entry::Vacant(e) => {
            let instance = spawn(key, init_req_params, &options, map)
                .await
                .context("spawning instance")?;
            e.insert(instance.clone());
            Ok(instance)
        }
    }
}

#[instrument(name = "instance", fields(pid = field::Empty), skip_all, parent = None)]
async fn spawn(
    key: InstanceKey,
    mut init_req_params: lsp::InitializeParams,
    options: &InstanceOptions,
    // Caller `get_or_spawn` is holding a lock to the map, we must not try to
    // lock it within this function to not cause deadlock, only spawned tasks
    // are allowed to lock it again.
    map: Arc<Mutex<InstanceMap>>,
) -> Result<Arc<Instance>> {
    let mut child = Command::new(&key.server)
        .args(&key.args)
        .envs(&key.env)
        .current_dir(&key.workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            let InstanceKey {
                server,
                args,
                env,
                workspace_root,
            } = &key;
            let path = env
                .get("PATH")
                .map(<_>::to_owned)
                // Display PATH from our environment Command will if none was
                // passed from the client environment.
                .or_else(|| env::var("PATH").ok())
                .unwrap_or_default();
            format!(
                "spawning language server: server={server:?}, args={args:?}, \
                cwd={workspace_root:?}, path={path:?}, env={env:?}",
            )
        })?;

    let pid = child.id().context("child exited early, couldn't get PID")?;
    tracing::Span::current().record("pid", pid);

    info!(server = ?key.server, args = ?key.args, cwd = ?key.workspace_root, "spawned language server");

    let stderr = child.stderr.take().unwrap();
    task::spawn(stderr_task(stderr).in_current_span());

    let stdout = child.stdout.take().unwrap();
    let mut reader = LspReader::new(BufReader::new(stdout), "server");

    let stdin = child.stdin.take().unwrap();
    let mut writer = LspWriter::new(stdin, "server");

    let workspace_folders = initial_workspace_folders(&init_req_params);

    let settings =
        options
            .server_settings
            .get(&key.server)
            .and_then(|path| match settings::load(path) {
                Ok(settings) => {
                    info!(path, "lspmux owns the server settings");
                    Some(settings)
                }
                Err(err) => {
                    error!(?err, "loading server settings, leaving them to the clients");
                    None
                }
            });

    capabilities::force_mux_capabilities(&mut init_req_params.capabilities);
    if let Some(settings) = &settings {
        init_req_params.initialization_options = Some(lsp::InitializationOptions {
            lsp_mux: None,
            other_options: settings.clone(),
        });
    }
    let settings = settings.map(Value::Object);

    let mut init_result =
        initialize_handshake(init_req_params, settings.as_ref(), &mut reader, &mut writer)
            .await
            .context("server handshake")?;

    info!("initialized server");

    // Clients send us full text changes, we keep their copies and decide
    // what the server sees.
    let encoding = PositionEncoding::from_server_capabilities(&init_result.capabilities);
    docs::rewrite_text_document_sync(&mut init_result.capabilities);

    let (message_writer, rx) = mpsc::channel(64);

    let instance = Arc::new(Instance {
        key,
        pid,
        init_result,
        workspace_folders,
        server: message_writer,
        clients: Mutex::default(),
        dynamic_capabilities: Mutex::default(),
        docs: Mutex::new(Docs::new(encoding)),
        progress: Mutex::new(Progress::new(Instant::now())),
        settle: options.settle,
        settings,
        request_snapshots: Mutex::default(),
        execute_commands: Mutex::default(),
        close: Notify::new(),
        last_used: AtomicI64::new(utc_now()),
    });

    task::spawn(stdout_task(instance.clone(), reader).in_current_span());
    task::spawn(stdin_task(rx, writer).in_current_span());
    task::spawn(settle_task(Arc::downgrade(&instance)).in_current_span());

    task::spawn(wait_task(instance.clone(), map, child).in_current_span());

    Ok(instance)
}

#[instrument(skip_all)]
async fn initialize_handshake(
    init_req_params: lsp::InitializeParams,
    settings: Option<&Value>,
    reader: &mut LspReader<BufReader<ChildStdout>>,
    writer: &mut LspWriter<ChildStdin>,
) -> Result<lsp::InitializeResult> {
    let request_id = "lspmux:initialize_request";

    // Use the first client's `InitializeParams` to initialize server. We assume
    // all subsequent clients configuration will be somewhat compatible with
    // whatever the first client negotiated for the same `workspace_root`,
    // `server` and `args`.
    let req = Request {
        jsonrpc: Version,
        method: "initialize".into(),
        params: serde_json::to_value(init_req_params).unwrap(),
        id: RequestId::String(request_id.into()),
    };
    writer
        .write_message(&req.into())
        .await
        .context("send initialize request")?;

    let res = match reader
        .read_message()
        .await
        .context("receive initialize response")?
        .context("stream ended")?
    {
        Message::ResponseSuccess(res) if res.id == request_id => res,
        _ => bail!("first server message was not initialize response"),
    };
    let result = serde_json::from_value(res.result).context("parse initialize response result")?;

    // Send a "fake" `initialized` notification to the server. We wait for them
    // from each client's `initialized` notification ourselves and they don't
    // contain any data that would need passing on.
    let init_notif = Notification {
        jsonrpc: Version,
        method: "initialized".into(),
        params: json!({}),
    };
    writer
        .write_message(&init_notif.into())
        .await
        .context("send initialized notification")?;

    if let Some(settings) = settings {
        // Pushed once, some servers don't ask with `workspace/configuration`
        let notif = Notification {
            jsonrpc: Version,
            method: "workspace/didChangeConfiguration".into(),
            params: json!({ "settings": settings }),
        };
        writer
            .write_message(&notif.into())
            .await
            .context("send workspace/didChangeConfiguration notification")?;
    }

    Ok(result)
}

/// Answer owed diagnostics once the server settled, until the instance is gone
async fn settle_task(instance: Weak<Instance>) {
    let Some(settle) = instance.upgrade().map(|instance| instance.settle) else {
        return;
    };
    let mut interval = tokio::time::interval((settle / 4).max(Duration::from_millis(1)));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let Some(instance) = instance.upgrade() else {
            break;
        };
        instance.answer_owed_diagnostics().await;
    }
    debug!("settle task stopped");
}

/// Read errors from language server stderr and log them
async fn stderr_task(stderr: ChildStderr) {
    let mut stderr = BufReader::new(stderr);
    let mut buffer = String::new();

    loop {
        buffer.clear();
        match stderr.read_line(&mut buffer).await {
            Ok(0) => {
                // reached EOF
                debug!("stderr closed");
                break;
            }
            Ok(_) => {
                let line = buffer.trim_end(); // remove trailing '\n' or possibly '\r\n'
                error!(%line, "stderr");
            }
            Err(err) => {
                let err = anyhow::Error::from(err);
                error!(?err, "error reading from stderr");
            }
        }
    }
}

/// Receive messages from clients' channel and write them into language server stdin
async fn stdin_task(mut receiver: mpsc::Receiver<Message>, mut writer: LspWriter<ChildStdin>) {
    // Because we (stdin task) don't keep a reference to `self` it will be dropped when the
    // child closes and all the clients disconnect including the sender and this receiver
    // will not keep blocking (unlike in client input task)
    while let Some(message) = receiver.recv().await {
        if let Err(err) = writer.write_message(&message).await {
            match err.kind() {
                // stdin is closed, no need to log an error
                ErrorKind::BrokenPipe => {}
                _ => {
                    let err = anyhow::Error::from(err);
                    error!(?err, "error writing to stdin");
                }
            }
            break;
        }
    }
    debug!("stdin closed");
}

/// Wait for child and log when it exits
async fn wait_task(
    instance: Arc<Instance>,
    instance_map: Arc<Mutex<InstanceMap>>,
    mut child: Child,
) {
    let key = instance.key.clone();
    loop {
        select! {
            _ = instance.close.notified() => {
                if let Err(err) = child.start_kill() {
                    error!(?err, "failed to close child");
                }
            }
            exit = child.wait() => {
                // Remove the closing instance from the map so new clients spawn their own instance
                instance_map.lock().await.instances.remove(&key);

                // Disconnect all current clients
                //
                // We'll rely on the editor client to restart the lspmux client,
                // start a new connection and we'll spawn another instance like
                // we'd with any other new client.
                instance.clients.lock().await.clear();

                match exit {
                    Ok(status) => {
                        #[cfg(unix)]
                        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
                        #[cfg(not(unix))]
                        let signal = tracing::field::Empty;

                        error!(
                            success = status.success(),
                            code = status.code(),
                            signal,
                            "child exited",
                        );
                    }
                    Err(err) => error!(?err, "error waiting for child"),
                }
                break;
            }
        }
    }
}

/// Workspace folders the server is initialized with, as the
/// `workspace/workspaceFolders` result
///
/// Falls back to a single folder from the deprecated `rootUri` and `null`
/// (no workspace) when neither is present.
fn initial_workspace_folders(params: &lsp::InitializeParams) -> Value {
    if !params.workspace_folders.is_empty() {
        return serde_json::to_value(&params.workspace_folders).unwrap();
    }
    match &params.root_uri {
        Some(uri) => {
            let name = uri.trim_end_matches('/').rsplit('/').next().unwrap_or(uri);
            json!([{ "uri": uri, "name": name }])
        }
        None => Value::Null,
    }
}

/// Read messages from server stdout and send them to corresponding client channels
async fn stdout_task(instance: Arc<Instance>, mut reader: LspReader<BufReader<ChildStdout>>) {
    loop {
        let message = match reader.read_message().await {
            Ok(Some(message)) => message,
            Ok(None) => {
                debug!("stdout closed");
                break;
            }
            Err(err) => {
                error!(?err, "reading message");
                continue;
            }
        };

        // Lock _after_ we have a message to send, then send and immediately release the lock
        let clients = instance.clients.lock().await;
        match message {
            Message::ResponseSuccess(mut res) => {
                // Forward successful response to the right client based on the
                // Request ID tag.
                match res.id.untag() {
                    (Some(Tag::ClientId(client_id)), id) => {
                        let snapshot = instance.take_request_snapshot(&res.id).await;
                        instance.finish_execute_command(&res.id).await;
                        res.id = id;
                        // Must be based on the client's text, document
                        // versions must be in the client's numbering
                        let res = response_for_client(
                            &*instance.docs.lock().await,
                            client_id,
                            &snapshot,
                            res,
                            &read_file_uri,
                        );
                        if let Some(client) = clients.get(&client_id) {
                            let _ = client.send_message(res).await;
                        } else {
                            debug!(?client_id, "no matching client");
                        }
                    }
                    (Some(Tag::Drop), _) => {
                        // Drop the message
                    }
                    _ => {
                        warn!(?res, "ignoring improperly tagged server response")
                    }
                }
            }

            Message::ResponseError(mut res) => {
                // Forward the error response to the right client based on the
                // Request ID tag.
                match res.id.untag() {
                    (Some(Tag::ClientId(client_id)), id) => {
                        warn!(?res, "server responded with error");
                        instance.take_request_snapshot(&res.id).await;
                        instance.finish_execute_command(&res.id).await;
                        res.id = id;
                        if let Some(client) = clients.get(&client_id) {
                            let _ = client.send_message(res.into()).await;
                        } else {
                            debug!(?client_id, "no matching client");
                        }
                    }
                    (Some(Tag::Drop), _) => {
                        // Drop the message
                    }
                    _ => {
                        warn!(?res, "ignoring improperly tagged server response")
                    }
                }
            }

            Message::Request(mut req) if req.method == "window/workDoneProgress/create" => {
                // It has a null response and we need to inform all clients.
                // We can forward the request to all clients, send a fake
                // successful response and ignore the real client responses.
                trace!(?req, "server request window/workDoneProgress/create");

                // Clients attaching later get it replayed
                instance.progress.lock().await.create(&req.params);

                let id = req.id;
                req.id = id.tag(Tag::Drop);

                let deliveries =
                    translated_for_each(&*instance.docs.lock().await, &clients, &req.params);
                for (client_id, params) in deliveries {
                    let req = Request {
                        params,
                        ..req.clone()
                    };
                    let _ = clients[&client_id].send_message(req.into()).await;
                }

                let _ = instance
                    .send_message(ResponseSuccess::null(id).into())
                    .await;
            }

            Message::Request(mut req) if REFRESH_REQUESTS.contains(&req.method.as_str()) => {
                // Null response, answered by us. Only clients which declared
                // support for it get it, their responses are ignored.
                trace!(?req, "server request {}", req.method.as_str());

                let id = req.id;
                req.id = id.tag(Tag::Drop);

                let capability = refresh_capability(&req.method).unwrap_or_default();
                for client in clients.values() {
                    if client.record().declares(&capability) {
                        let _ = client.send_message(req.clone().into()).await;
                    }
                }

                let _ = instance
                    .send_message(ResponseSuccess::null(id).into())
                    .await;
            }

            Message::Request(req) if req.method == "workspace/configuration" => {
                debug!(?req, "server request workspace/configuration");

                if let Some(settings) = &instance.settings {
                    let res = ResponseSuccess {
                        jsonrpc: Version,
                        result: settings::configuration_result(settings, &req.params),
                        id: req.id,
                    };
                    let _ = instance.send_message(res.into()).await;
                } else if let Some(client) = clients
                    .values()
                    .find(|client| client.record().declares("workspace.configuration"))
                {
                    // Response to `workspace/configuration` should be the same
                    // from any client. So we'll just pick one and let it answer.
                    match request_for_client(&*instance.docs.lock().await, client.id(), req) {
                        Ok(mut req) => {
                            req.id = req.id.tag(Tag::Forward);
                            let _ = client.send_message(req.into()).await;
                        }
                        Err(reply) => {
                            warn!(?reply, "server request unsafe for the client");
                            let _ = instance.send_message(reply).await;
                        }
                    }
                } else {
                    // Nobody to ask, the server would wait forever
                    let res = ResponseSuccess {
                        jsonrpc: Version,
                        result: settings::null_configuration_result(&req.params),
                        id: req.id,
                    };
                    let _ = instance.send_message(res.into()).await;
                }
            }

            Message::Request(req) if req.method == "client/registerCapability" => {
                // Forwarded to every client which supports dynamic
                // registration of the capability, and cached for clients
                // coming later. The response doesn't contain anything
                // important so we can safely ignore the real answers and send
                // a fake one to the server.
                debug!(?req, "server request client/registerCapability");

                match serde_json::from_value::<lsp::RegistrationParams>(req.params.clone()) {
                    Ok(params) => {
                        let deliveries = instance
                            .dynamic_capabilities
                            .lock()
                            .await
                            .register(params.registrations);
                        for (client_id, registrations) in deliveries {
                            let req = Request {
                                jsonrpc: Version,
                                method: req.method.clone(),
                                params: serde_json::to_value(lsp::RegistrationParams {
                                    registrations,
                                })
                                .unwrap(),
                                id: req.id.tag(Tag::Drop),
                            };
                            if let Some(client) = clients.get(&client_id) {
                                let _ = client.send_message(req.into()).await;
                            }
                        }
                    }
                    Err(err) => warn!(?err, "error parsing client/registerCapability params"),
                }

                let _ = instance
                    .send_message(ResponseSuccess::null(req.id).into())
                    .await;
            }

            Message::Request(req) if req.method == "client/unregisterCapability" => {
                // Forwarded to the clients which received the registrations,
                // the cache stops announcing them to new clients.
                debug!(?req, "server request client/unregisterCapability");

                match serde_json::from_value::<lsp::UnregistrationParams>(req.params.clone()) {
                    Ok(params) => {
                        let deliveries = instance
                            .dynamic_capabilities
                            .lock()
                            .await
                            .unregister(params.unregistrations);
                        for (client_id, unregistrations) in deliveries {
                            let req = Request {
                                jsonrpc: Version,
                                method: req.method.clone(),
                                params: serde_json::to_value(lsp::UnregistrationParams {
                                    unregistrations,
                                })
                                .unwrap(),
                                id: req.id.tag(Tag::Drop),
                            };
                            if let Some(client) = clients.get(&client_id) {
                                let _ = client.send_message(req.into()).await;
                            }
                        }
                    }
                    Err(err) => warn!(?err, "error parsing client/unregisterCapability params"),
                }

                let _ = instance
                    .send_message(ResponseSuccess::null(req.id).into())
                    .await;
            }

            Message::Request(req) if req.method == "workspace/applyEdit" => {
                // The edit must be applied exactly once, by the client whose
                // `workspace/executeCommand` caused it. When that's ambiguous
                // or the edit may be based on text the client doesn't have we
                // refuse it instead of guessing. The client response is
                // forwarded back to the server.
                debug!(?req, "server request workspace/applyEdit");

                let routed = {
                    let docs = instance.docs.lock().await;
                    let execute_commands = instance.execute_commands.lock().await;
                    let owners = execute_commands.owners();
                    let uris = workspace_edit_uris(&req.params["edit"]);
                    let selected = select_request_owner(&owners).and_then(|client_id| {
                        clients
                            .get(&client_id)
                            .map(|client| (client_id, client))
                            .ok_or("the client which triggered the request disconnected")
                    });
                    match selected {
                        Ok((client_id, client)) => check_apply_edit(
                            &docs,
                            client_id,
                            execute_commands.snapshots_of(client_id),
                            &uris,
                            &read_file_uri,
                        )
                        .map_err(|reason| {
                            Message::from(apply_edit_failure(req.id.clone(), &reason))
                        })
                        .and_then(|()| request_for_client(&docs, client_id, req))
                        .map(|req| (client_id, client, req)),
                        Err(reason) => Err(apply_edit_failure(req.id, reason).into()),
                    }
                };

                match routed {
                    Ok((client_id, client, mut req)) => {
                        debug!(
                            ?client_id,
                            "forwarding workspace/applyEdit to the client which triggered it"
                        );
                        req.id = req.id.tag(Tag::Forward);
                        let _ = client.send_message(req.into()).await;
                    }
                    Err(reply) => {
                        warn!(?reply, "rejecting workspace/applyEdit");
                        let _ = instance.send_message(reply).await;
                    }
                }
            }

            Message::Request(req) if req.method == "workspace/workspaceFolders" => {
                // We know what the instance was initialized with, answer
                // ourselves.
                debug!(?req, "server request workspace/workspaceFolders");
                let res = ResponseSuccess {
                    jsonrpc: Version,
                    result: instance.workspace_folders.clone(),
                    id: req.id,
                };
                let _ = instance.send_message(res.into()).await;
            }

            Message::Request(req)
                if req.method == "window/showDocument"
                    || req.method == "window/showMessageRequest" =>
            {
                // Only the client whose command caused these can answer them.
                debug!(?req, "server request {}", req.method.as_str());

                let owner = {
                    let owners = instance.execute_commands.lock().await.owners();
                    select_request_owner(&owners).and_then(|client_id| {
                        clients
                            .get(&client_id)
                            .map(|client| (client_id, client))
                            .ok_or("the client which triggered the request disconnected")
                    })
                };

                match owner {
                    Ok((client_id, client)) => {
                        match request_for_client(&*instance.docs.lock().await, client_id, req) {
                            Ok(mut req) => {
                                debug!(
                                    ?client_id,
                                    "forwarding {} to the client which triggered it", req.method
                                );
                                req.id = req.id.tag(Tag::Forward);
                                let _ = client.send_message(req.into()).await;
                            }
                            Err(reply) => {
                                warn!(?reply, "server request unsafe for the client");
                                let _ = instance.send_message(reply).await;
                            }
                        }
                    }
                    Err(reason) if req.method == "window/showDocument" => {
                        warn!(reason, "rejecting window/showDocument");
                        let res = ResponseSuccess {
                            jsonrpc: Version,
                            result: json!({ "success": false }),
                            id: req.id,
                        };
                        let _ = instance.send_message(res.into()).await;
                    }
                    Err(reason) => {
                        // Show the message to everyone, nobody gets to pick
                        // an action.
                        debug!(
                            reason,
                            "broadcasting window/showMessageRequest as window/showMessage"
                        );
                        let notif = Notification {
                            jsonrpc: Version,
                            method: "window/showMessage".into(),
                            params: json!({
                                "type": req.params["type"],
                                "message": req.params["message"],
                            }),
                        };
                        for client in clients.values() {
                            let _ = client.send_message(notif.clone().into()).await;
                        }
                        let _ = instance
                            .send_message(ResponseSuccess::null(req.id).into())
                            .await;
                    }
                }
            }

            Message::Request(req) => {
                // Answer instead of leaving the server waiting forever.
                warn!(?req, "unsupported server request");
                let res = ResponseError {
                    jsonrpc: Version,
                    error: jsonrpc::Error {
                        code: -32601,
                        message: format!("lspmux: unsupported server request {}", req.method),
                        data: None,
                    },
                    id: req.id,
                };
                let _ = instance.send_message(res.into()).await;
            }

            Message::Notification(notif) if notif.method == "textDocument/publishDiagnostics" => {
                // Cache them for clients connecting or opening the document
                // later and only show them to clients with matching text.
                let deliveries = {
                    let connected = clients.keys().copied().collect::<Vec<_>>();
                    let mut docs = instance.docs.lock().await;
                    instance.progress.lock().await.touch(Instant::now());
                    docs.publish_diagnostics(notif.params, &connected)
                };
                for (client_id, notif) in deliveries {
                    let _ = clients[&client_id].send_message(notif.into()).await;
                }
            }

            Message::Notification(notif) => {
                if notif.method == "$/progress" {
                    // Tracked for clients attaching later and settling
                    instance
                        .progress
                        .lock()
                        .await
                        .progress(&notif.params, Instant::now());
                }
                // Server notifications don't expect a response. We can forward
                // them to all clients, except those they're unsafe for.
                let deliveries =
                    translated_for_each(&*instance.docs.lock().await, &clients, &notif.params);
                for (client_id, params) in deliveries {
                    let notif = Notification {
                        params,
                        ..notif.clone()
                    };
                    let _ = clients[&client_id].send_message(notif.into()).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: usize = 1;
    const B: usize = 2;

    fn owner(client_id: usize) -> Owner {
        Owner {
            client_id,
            orphaned: false,
        }
    }

    fn orphan(client_id: usize) -> Owner {
        Owner {
            client_id,
            orphaned: true,
        }
    }

    #[test]
    fn routing_single_connected_owner() {
        assert_eq!(select_request_owner(&[owner(A)]), Ok(A));
    }

    #[test]
    fn routing_several_owners_is_ambiguous() {
        // A's and B's commands are both in flight and an edit arrives which
        // touches a file only B has open. A's command may legitimately edit a
        // file A doesn't have open, so open documents don't establish
        // ownership: neither client may receive it.
        let tracker = ExecuteCommandTracker {
            in_flight: vec![
                InFlightCommand {
                    key: "1".into(),
                    client_id: A,
                    orphaned: false,
                    snapshot: Snapshot::default(),
                },
                InFlightCommand {
                    key: "2".into(),
                    client_id: B,
                    orphaned: false,
                    snapshot: Snapshot::default(),
                },
            ],
        };
        assert!(select_request_owner(&tracker.owners()).is_err());
        // arrival order doesn't matter either
        assert!(select_request_owner(&[owner(B), owner(A)]).is_err());
    }

    #[test]
    fn routing_fails_closed() {
        // sole owner orphaned
        assert!(select_request_owner(&[orphan(A)]).is_err());
        // orphan plus a connected owner
        assert!(select_request_owner(&[orphan(A), owner(B)]).is_err());
        // no owner
        assert!(select_request_owner(&[]).is_err());
    }

    #[test]
    fn tracker_owners_survive_disconnect() {
        let command = |key: &str, client_id, orphaned| InFlightCommand {
            key: key.into(),
            client_id,
            orphaned,
            snapshot: Snapshot::default(),
        };
        let tracker = ExecuteCommandTracker {
            in_flight: vec![
                command("1", A, false),
                command("2", B, true),
                command("3", A, false),
            ],
        };
        assert_eq!(tracker.owners(), [owner(A), orphan(B)]);
    }

    #[test]
    fn edited_uris() {
        let edit = json!({
            "changes": { "file:///a": [] },
            "documentChanges": [
                { "textDocument": { "uri": "file:///b", "version": 1 }, "edits": [] },
                { "kind": "create", "uri": "file:///c" },
                { "kind": "rename", "oldUri": "file:///d", "newUri": "file:///e" },
                { "kind": "delete", "uri": "file:///f" },
            ],
        });
        assert_eq!(
            workspace_edit_uris(&edit),
            [
                "file:///a",
                "file:///b",
                "file:///c",
                "file:///d",
                "file:///e",
                "file:///f"
            ]
        );

        // WorkspaceEdits nested in results, e.g. code actions
        let actions = json!([
            { "title": "x", "edit": { "changes": { "file:///g": [] } } },
            { "title": "y", "command": { "command": "c", "arguments": [{ "changes": "not a map" }] } },
            { "title": "z", "edit": { "documentChanges": [{ "textDocument": { "uri": "file:///h", "version": null }, "edits": [] }] } },
        ]);
        assert_eq!(workspace_edit_uris(&actions), ["file:///g", "file:///h"]);
        // applyEdit params
        assert_eq!(
            workspace_edit_uris(
                &json!({ "label": "l", "edit": { "changes": { "file:///i": [] } } })
            ),
            ["file:///i"]
        );
    }

    const U: &str = "file:///p/A.hs";

    /// Client A (version 10) and B (version 20) diverged, server at version 2
    /// with B's text, A matched server version 1
    fn diverged_docs() -> Docs {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        let open = |version: i64, text: &str| json!({ "textDocument": { "uri": U, "languageId": "haskell", "version": version, "text": text } });
        docs.did_open(A, &open(10, "a")).unwrap();
        docs.did_open(B, &open(20, "b")).unwrap();
        docs
    }

    fn apply_edit(version: i64) -> Request {
        Request {
            jsonrpc: Version,
            method: "workspace/applyEdit".into(),
            params: json!({ "edit": { "documentChanges": [
                { "textDocument": { "uri": U, "version": version }, "edits": [] },
            ] } }),
            id: RequestId::Number(7),
        }
    }

    #[test]
    fn apply_edit_with_unsafe_version_is_refused() {
        let mut docs = diverged_docs();
        // A changed since server version 1
        docs.did_change(
            A,
            &json!({ "textDocument": { "uri": U, "version": 11 }, "contentChanges": [{ "text": "aa" }] }),
        )
        .unwrap();
        docs.sync_for_request(B, &json!({}));

        let reply = request_for_client(&docs, B, apply_edit(1)).err().unwrap();
        assert_eq!(
            serde_json::to_value(reply).unwrap(),
            json!({ "jsonrpc": "2.0", "id": 7, "result": {
                "applied": false,
                "failureReason": format!("lspmux: edit targets a document version client {B} no longer has"),
            } })
        );

        // A's edit for server version 1 is unsafe too, it changed since
        assert!(request_for_client(&docs, A, apply_edit(1)).is_err());
        // B's text is the server's current one (version 4 after the resyncs)
        let req = request_for_client(&docs, B, apply_edit(4)).ok().unwrap();
        assert_eq!(
            req.params["edit"]["documentChanges"][0]["textDocument"]["version"],
            20
        );
    }

    #[test]
    fn apply_edit_with_older_matched_version_is_forwarded() {
        let docs = diverged_docs();
        // A didn't change since the server had its text at version 1
        let req = request_for_client(&docs, A, apply_edit(1)).ok().unwrap();
        assert_eq!(
            req.params["edit"]["documentChanges"][0]["textDocument"]["version"],
            10
        );
    }

    #[test]
    fn response_with_unsafe_version_becomes_content_modified() {
        let docs = diverged_docs();
        let res = |version: i64| ResponseSuccess {
            jsonrpc: Version,
            result: json!([{ "textDocument": { "uri": U, "version": version } }]),
            id: RequestId::Number(3),
        };

        let unsafe_res = serde_json::to_value(response_for_client(
            &docs,
            A,
            &Snapshot::default(),
            res(2),
            &no_disk,
        ))
        .unwrap();
        assert_eq!(
            unsafe_res,
            json!({ "jsonrpc": "2.0", "id": 3, "error": {
                "code": -32801,
                "message": "lspmux: result refers to a document version this client no longer has",
            } })
        );

        let safe_res = serde_json::to_value(response_for_client(
            &docs,
            B,
            &Snapshot::default(),
            res(2),
            &no_disk,
        ))
        .unwrap();
        assert_eq!(safe_res["result"][0]["textDocument"]["version"], 20);
    }

    fn no_disk(_: &str) -> Option<String> {
        panic!("no disk access expected")
    }

    fn open(uri: &str, version: i64, text: &str) -> Value {
        json!({ "textDocument": { "uri": uri, "languageId": "haskell", "version": version, "text": text } })
    }

    fn change(uri: &str, version: i64, text: &str) -> Value {
        json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text }] })
    }

    #[test]
    fn unversioned_apply_edit_checked_against_command_snapshot() {
        let uris = [U.to_owned()];
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(A, &open(U, 1, "a")).unwrap();
        docs.did_open(B, &open(U, 1, "a")).unwrap();
        let snapshot = docs.snapshot(A);

        // unchanged
        assert_eq!(
            check_apply_edit(&docs, A, [&snapshot], &uris, &no_disk),
            Ok(())
        );
        // no command in flight
        assert!(check_apply_edit(&docs, A, [], &uris, &no_disk).is_err());

        // A's own edit during the command
        docs.did_change(A, &change(U, 2, "a2")).unwrap();
        let err = check_apply_edit(&docs, A, [&snapshot], &uris, &no_disk).unwrap_err();
        assert!(err.contains("changed by the client"), "{err}");

        // B switches the server to its text during the command
        let snapshot = docs.snapshot(A);
        assert_eq!(
            check_apply_edit(&docs, A, [&snapshot], &uris, &no_disk),
            Ok(())
        );
        docs.did_change(B, &change(U, 2, "b")).unwrap();
        let err = check_apply_edit(&docs, A, [&snapshot], &uris, &no_disk).unwrap_err();
        assert!(err.contains("another client's text"), "{err}");
        let reply = serde_json::to_value(apply_edit_failure(RequestId::Number(7), &err)).unwrap();
        assert_eq!(reply["result"]["applied"], false);
        assert!(reply["result"]["failureReason"]
            .as_str()
            .unwrap()
            .starts_with("lspmux: "));

        // with several commands in flight every snapshot must be safe
        let newer = docs.snapshot(A);
        assert_eq!(
            check_apply_edit(&docs, A, [&newer], &uris, &no_disk),
            Ok(())
        );
        assert!(check_apply_edit(&docs, A, [&snapshot, &newer], &uris, &no_disk).is_err());
    }

    #[test]
    fn unversioned_apply_edit_of_document_not_open_in_client() {
        const O: &str = "file:///p/Other.hs";
        let uris = [O.to_owned()];
        let disk = |uri: &str| (uri == O).then(|| "on disk".to_owned());
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(A, &open(U, 1, "a")).unwrap();
        let snapshot = docs.snapshot(A);

        // nobody has it open
        assert_eq!(
            check_apply_edit(&docs, A, [&snapshot], &uris, &no_disk),
            Ok(())
        );
        // B has it open with the disk text
        docs.did_open(B, &open(O, 1, "on disk")).unwrap();
        assert_eq!(
            check_apply_edit(&docs, A, [&snapshot], &uris, &disk),
            Ok(())
        );
        // B's text differs from disk
        docs.did_change(B, &change(O, 2, "unsaved")).unwrap();
        let err = check_apply_edit(&docs, A, [&snapshot], &uris, &disk).unwrap_err();
        assert!(err.contains("differs from disk"), "{err}");
    }

    fn code_action(uri: &str) -> ResponseSuccess {
        ResponseSuccess {
            jsonrpc: Version,
            result: json!([{ "title": "t", "edit": { "changes": { uri: [] } } }]),
            id: RequestId::Number(3),
        }
    }

    fn is_foreign_text_error(message: &Message) -> bool {
        serde_json::to_value(message).unwrap()["error"]
            == json!({ "code": -32801, "message": FOREIGN_TEXT })
    }

    #[test]
    fn response_checked_against_request_snapshot() {
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(A, &open(U, 1, "a")).unwrap();
        docs.did_open(B, &open(U, 1, "a")).unwrap();

        // unchanged
        let snapshot = docs.snapshot(A);
        let res = response_for_client(&docs, A, &snapshot, code_action(U), &no_disk);
        assert!(matches!(res, Message::ResponseSuccess(_)));

        // A's own edit doesn't reject a response
        docs.did_change(A, &change(U, 2, "a2")).unwrap();
        let res = response_for_client(&docs, A, &snapshot, code_action(U), &no_disk);
        assert!(matches!(res, Message::ResponseSuccess(_)));

        // B switching the server to its text does, for any response
        let snapshot = docs.snapshot(A);
        docs.did_change(B, &change(U, 2, "b")).unwrap();
        let res = response_for_client(&docs, A, &snapshot, code_action(U), &no_disk);
        assert!(is_foreign_text_error(&res));
        let plain = ResponseSuccess::null(RequestId::Number(3));
        assert!(is_foreign_text_error(&response_for_client(
            &docs, A, &snapshot, plain, &no_disk
        )));
    }

    #[test]
    fn response_editing_document_open_in_another_client() {
        const O: &str = "file:///p/Other.hs";
        let disk = |uri: &str| (uri == O).then(|| "on disk".to_owned());
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(A, &open(U, 1, "a")).unwrap();
        let snapshot = docs.snapshot(A);

        // nobody has it open
        let res = response_for_client(&docs, A, &snapshot, code_action(O), &no_disk);
        assert!(matches!(res, Message::ResponseSuccess(_)));
        // B has the disk text open
        docs.did_open(B, &open(O, 1, "on disk")).unwrap();
        let res = response_for_client(&docs, A, &snapshot, code_action(O), &disk);
        assert!(matches!(res, Message::ResponseSuccess(_)));
        // B's text differs from disk
        docs.did_change(B, &change(O, 2, "unsaved")).unwrap();
        let res = response_for_client(&docs, A, &snapshot, code_action(O), &disk);
        assert!(is_foreign_text_error(&res));
    }

    #[test]
    fn owed_diagnostics_wait_for_the_server_to_settle() {
        let settle = Duration::from_millis(1500);
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut progress = Progress::new(start);
        let mut docs = Docs::new(PositionEncoding::Utf16);
        docs.did_open(A, &open(U, 1, "broken")).unwrap();
        let cached = json!({ "uri": U, "version": 1, "diagnostics": [{ "message": "err" }] });
        docs.publish_diagnostics(cached, &[A]);

        // re-sending the same text reaches neither the server nor a publish
        let effects = docs.did_change(A, &change(U, 2, "broken")).unwrap();
        assert!(effects.to_server.is_empty());
        assert!(owed_when_settled(&mut docs, &progress, at(1000), settle).is_empty());

        // activity before settling, e.g. another document's change, delays it
        progress.touch(at(1000));
        assert!(owed_when_settled(&mut docs, &progress, at(2000), settle).is_empty());

        // so does server progress for as long as it's active
        progress.progress(
            &json!({ "token": 1, "value": { "kind": "begin", "title": "Processing" } }),
            at(2000),
        );
        assert!(owed_when_settled(&mut docs, &progress, at(10_000), settle).is_empty());
        progress.progress(
            &json!({ "token": 1, "value": { "kind": "end" } }),
            at(10_000),
        );
        assert!(owed_when_settled(&mut docs, &progress, at(11_000), settle).is_empty());

        let answers = owed_when_settled(&mut docs, &progress, at(11_500), settle);
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].0, A);
        assert_eq!(
            answers[0].1.params,
            json!({ "uri": U, "version": 2, "diagnostics": [{ "message": "err" }] })
        );
        // answered once
        assert!(owed_when_settled(&mut docs, &progress, at(20_000), settle).is_empty());
    }
}
