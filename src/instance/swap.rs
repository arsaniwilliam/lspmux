//! Replacing the language server process behind connected clients
//!
//! See [`Instance::recycle`] for the procedure and [`crate::recycle`] for the
//! triggers. The process is replaced while the gate of the instance is held
//! for writing: client messages arriving meanwhile wait for it, and reach
//! the new server after its documents were reopened.

use anyhow::ensure;
use tokio::time::{interval_at, timeout, timeout_at};
use tracing::{info_span, Span};

use super::*;
use crate::config::RecycleConfig;
use crate::recycle::{Decision, Guard, Level};

/// Message of the error answering requests the replaced server never answered
const RESTARTED: &str = "lspmux: language server restarted";

/// How long the old process gets to exit after `shutdown` and `exit`
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a killed process gets to disappear
const KILL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a recycle waits for everything to leave the gate before it
/// assumes the server isn't reading its input and kills it
const GATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Server requests the clients are asked to repeat after a recycle, so
/// results of the old process are fetched again
const REFRESH: [&str; 4] = [
    "workspace/codeLens/refresh",
    "workspace/semanticTokens/refresh",
    "workspace/inlayHint/refresh",
    "workspace/diagnostic/refresh",
];

/// A started and initialized server process
pub(super) struct Process {
    pub child: Child,
    pub pid: u32,
    pub reader: LspReader<BufReader<ChildStdout>>,
    pub writer: LspWriter<ChildStdin>,
    pub init_result: lsp::InitializeResult,
    /// Tracing span of everything which belongs to the process
    pub span: Span,
}

/// Spawn the server, handle its stderr and run the `initialize` handshake
/// with `init_params` and `settings`
pub(super) async fn start_process(
    key: &InstanceKey,
    init_params: lsp::InitializeParams,
    settings: Option<&Value>,
    generation: u64,
    readings: &Arc<Readings>,
    wake: &Arc<Notify>,
) -> Result<Process> {
    let span = info_span!(parent: None, "instance", pid = field::Empty);
    start_process_in_span(key, init_params, settings, generation, readings, wake)
        .instrument(span)
        .await
}

async fn start_process_in_span(
    key: &InstanceKey,
    init_params: lsp::InitializeParams,
    settings: Option<&Value>,
    generation: u64,
    readings: &Arc<Readings>,
    wake: &Arc<Notify>,
) -> Result<Process> {
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
            } = key;
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
    Span::current().record("pid", pid);

    info!(server = ?key.server, args = ?key.args, cwd = ?key.workspace_root, generation, "spawned language server");

    let stderr = child.stderr.take().unwrap();
    task::spawn(
        stderr_task(stderr, generation, readings.clone(), wake.clone()).in_current_span(),
    );

    let stdout = child.stdout.take().unwrap();
    let mut reader = LspReader::new(BufReader::new(stdout), "server");

    let stdin = child.stdin.take().unwrap();
    let mut writer = LspWriter::new(stdin, "server");

    let init_result = match initialize_handshake(init_params, settings, &mut reader, &mut writer)
        .await
    {
        Ok(init_result) => init_result,
        Err(err) => {
            // Nobody else knows about the process, don't leave it behind
            let _ = child.kill().await;
            return Err(err);
        }
    };
    info!("initialized server");

    Ok(Process {
        child,
        pid,
        reader,
        writer,
        init_result,
        span: Span::current(),
    })
}

/// Error responses for the client requests identified by the tagged ID keys
/// the replaced server never answered
fn restarted_responses(keys: impl IntoIterator<Item = String>) -> Vec<(usize, ResponseError)> {
    keys.into_iter()
        .filter_map(|key| match RequestId::String(key).untag() {
            (Some(Tag::ClientId(client_id)), id) => {
                Some((client_id, content_modified(id, RESTARTED)))
            }
            _ => None,
        })
        .collect()
}

impl Instance {
    /// Numbers of the current process, for logs
    fn sample_fields(&self) -> (Option<f64>, Option<f64>) {
        let sample = self.readings.sample();
        (sample.live_mb, sample.rss_mb)
    }

    /// Read `/proc/<pid>/status` of the current process
    fn poll_memory(&self) {
        let (generation, pid) = {
            let link = self.link();
            (link.generation, link.pid)
        };
        let rss_mb = recycle::read_proc_memory(pid).map(|memory| memory.total_mb());
        self.readings.record_rss(generation, rss_mb);
    }

    /// Is this a settled moment for a soft recycle: no client request and no
    /// `workspace/executeCommand` in flight, no server progress, and the
    /// server quiet for `settle_ms`
    async fn settled_for_recycle(&self, now: Instant) -> bool {
        if !self.progress.lock().await.settled(now, self.settle) {
            return false;
        }
        self.request_snapshots.lock().await.is_empty()
            && self.execute_commands.lock().await.in_flight.is_empty()
    }

    /// Everything the clients were waiting for from the process being
    /// replaced ends here
    ///
    /// - requests in flight are answered with an error
    /// - the commands in flight are forgotten, so edits the old server
    ///   still has in mind find no owner and fail closed
    /// - active progress ends
    /// - the dynamic registrations are withdrawn, so the clients hold
    ///   exactly the registrations of the new server once it made them
    ///
    /// Server requests waiting for a client's reply stay unanswered, the
    /// reply is dropped by its generation tag.
    async fn abandon_process(&self, generation: u64) {
        let clients = self.clients.lock().await;

        let keys = {
            let mut snapshots = self.request_snapshots.lock().await;
            snapshots.drain().map(|(key, _)| key).collect::<Vec<_>>()
        };
        for (client_id, error) in restarted_responses(keys) {
            if let Some(client) = clients.get(&client_id) {
                let _ = client.send_message(error.into()).await;
            }
        }
        self.execute_commands.lock().await.in_flight.clear();

        let ends = self.progress.lock().await.end_all();
        for message in ends {
            for client in clients.values() {
                let _ = client.send_message(message.clone()).await;
            }
        }

        let unregistrations = self.dynamic_capabilities.lock().await.unregister_all();
        for (client_id, unregistrations) in unregistrations {
            let Some(client) = clients.get(&client_id) else {
                continue;
            };
            let req = Request {
                jsonrpc: Version,
                method: "client/unregisterCapability".into(),
                params: serde_json::to_value(lsp::UnregistrationParams { unregistrations })
                    .unwrap(),
                id: RequestId::String(format!("recycle:{generation}:unregisterCapability"))
                    .tag(Tag::Drop),
            };
            let _ = client.send_message(req.into()).await;
        }
    }

    /// Ask the process to exit, `shutdown` then `exit`, and kill it when it
    /// doesn't within [`STOP_TIMEOUT`]. Returns after it exited.
    async fn stop_process(&self, ctl: &ProcessCtl, sender: &mpsc::Sender<Message>) -> Result<()> {
        ctl.expected_exit.store(true, Ordering::SeqCst);

        let shutdown = Request {
            jsonrpc: Version,
            method: "shutdown".into(),
            params: Value::Null,
            id: RequestId::String("lspmux:shutdown".into()),
        };
        let exit = Notification {
            jsonrpc: Version,
            method: "exit".into(),
            params: Value::Null,
        };
        let graceful = async {
            // Fails when the process is gone already
            let _ = sender.send(shutdown.into()).await;
            let _ = sender.send(exit.into()).await;
            ctl.exited.notified().await;
        };
        if timeout_at((Instant::now() + STOP_TIMEOUT).into(), graceful)
            .await
            .is_ok()
        {
            return Ok(());
        }

        warn!("server process didn't exit in {STOP_TIMEOUT:?}, killing it");
        ctl.kill.notify_one();
        timeout(KILL_TIMEOUT, ctl.exited.notified())
            .await
            .context("server process didn't exit after SIGKILL")
    }

    /// Replace the language server process, the clients stay connected
    ///
    /// The old and the new process are never alive at the same time:
    /// 1. client traffic and messages of the old server wait at the gate
    /// 2. requests in flight get `ContentModified`, tracked commands,
    ///    progress and dynamic registrations of the old server end
    /// 3. the old process gets `shutdown`, `exit`, and SIGKILL after 2 s;
    ///    its exit is expected, the instance and its clients stay
    /// 4. a new process is spawned and initialized with the stored
    ///    `initialize` params and the settings, its capabilities are compared
    /// 5. every document the server had open is opened in it with the next
    ///    server version
    /// 6. the new process becomes the current one, memory readings and
    ///    generation included, and the gate opens: waiting client messages
    ///    reach it in order, its registrations reach the clients
    /// 7. clients which declare support get the `refresh` requests so cached
    ///    results of the old process are fetched again
    ///
    /// An error leaves no usable server: the caller takes the instance down.
    pub(super) async fn recycle(self: &Arc<Self>, map: &Arc<Mutex<InstanceMap>>) -> Result<()> {
        let started = Instant::now();

        let (old_generation, old_pid, old_ctl, old_sender) = {
            let link = self.link();
            (
                link.generation,
                link.pid,
                link.ctl.clone(),
                link.sender.clone(),
            )
        };

        let _gate = match timeout(GATE_TIMEOUT, self.gate.write()).await {
            Ok(gate) => gate,
            Err(_) => {
                // A request is stuck sending into the server's input
                warn!("server isn't reading its input, killing it");
                old_ctl.expected_exit.store(true, Ordering::SeqCst);
                old_ctl.kill.notify_one();
                self.gate.write().await
            }
        };
        ensure!(!self.dead.load(Ordering::SeqCst), "instance is gone");

        let generation = old_generation + 1;
        self.abandon_process(generation).await;
        self.stop_process(&old_ctl, &old_sender).await?;
        drop(old_sender);
        info!(old_pid, "old server process exited");

        // Before the new process writes anything
        self.readings.recycled(generation);
        let process = start_process(
            &self.key,
            self.init_params.clone(),
            self.settings.as_ref(),
            generation,
            &self.readings,
            &self.wake,
        )
        .await
        .context("starting the new server process")?;
        let Process {
            child,
            pid,
            reader,
            writer,
            init_result,
            span,
        } = process;

        let mut capabilities = init_result.capabilities;
        docs::rewrite_text_document_sync(&mut capabilities);
        if self.pid_commands {
            commands::capabilities_for_clients(&mut capabilities, pid);
        }
        if capabilities != self.init_result.capabilities {
            // Clients keep the capabilities they got when they connected
            warn!(
                old = %self.init_result.capabilities,
                new = %capabilities,
                "the new server process has other capabilities than the clients were told",
            );
        }

        let (sender, rx) = mpsc::channel(64);
        let ctl = Arc::new(ProcessCtl::default());
        task::spawn(stdout_task(self.clone(), generation, pid, reader).instrument(span.clone()));
        task::spawn(stdin_task(rx, writer).instrument(span.clone()));
        task::spawn(wait_task(self.clone(), ctl.clone(), map.clone(), child).instrument(span));
        *self.link() = Link {
            generation,
            pid,
            sender: sender.clone(),
            ctl,
        };

        {
            let mut docs = self.docs.lock().await;
            let reopened = docs.reopen_all();
            info!(count = reopened.len(), "reopening documents in the new server process");
            for notif in reopened {
                debug!(?notif, "reopening server document");
                sender
                    .send(notif.into())
                    .await
                    .context("reopening documents in the new server process")?;
            }
        }
        self.progress.lock().await.restarted(Instant::now());

        {
            let clients = self.clients.lock().await;
            for method in REFRESH {
                let capability = refresh_capability(method).unwrap_or_default();
                for client in clients
                    .values()
                    .filter(|client| client.record().declares(&capability))
                {
                    let req = Request {
                        jsonrpc: Version,
                        method: method.into(),
                        params: Value::Null,
                        id: RequestId::String(format!("recycle:{generation}:{method}"))
                            .tag(Tag::Drop),
                    };
                    let _ = client.send_message(req.into()).await;
                }
            }
        }

        info!(
            old_pid,
            new_pid = pid,
            generation,
            took_ms = started.elapsed().as_millis() as u64,
            "recycled the server process"
        );
        Ok(())
    }
}

/// Watch the numbers of the server process and recycle it at the limits of
/// `config`, until the instance is gone. After the guard stopped recycling
/// it keeps reading the numbers for `lspmux status`.
///
/// Evaluates when the server logged heap statistics, every `poll` after
/// reading `/proc`, and on the settle tick, where a pending soft recycle
/// gets its settled moment.
pub(super) async fn monitor_task(
    instance: Weak<Instance>,
    config: RecycleConfig,
    poll: Duration,
    wake: Arc<Notify>,
) {
    let Some(settle) = instance.upgrade().map(|instance| instance.settle) else {
        return;
    };
    let settle = (settle / 4).max(Duration::from_millis(1));
    let start = tokio::time::Instant::now();
    let mut settle_tick = interval_at(start + settle, settle);
    settle_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut poll_tick = interval_at(start + poll, poll);
    poll_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut guard = Guard::default();
    let mut pending = false;
    loop {
        let polled = select! {
            _ = settle_tick.tick() => false,
            _ = poll_tick.tick() => true,
            _ = wake.notified() => false,
        };
        let Some(instance) = instance.upgrade() else {
            break;
        };
        if instance.dead.load(Ordering::SeqCst) {
            break;
        }
        if polled {
            instance.poll_memory();
        }

        let sample = instance.readings.sample();
        let level = recycle::level(&config, &sample);
        let now = Instant::now();
        let settled = level == Some(Level::Soft) && instance.settled_for_recycle(now).await;
        let level = match guard.decide(level, settled, now) {
            Decision::Idle => {
                pending = false;
                continue;
            }
            Decision::Pending => {
                if !pending {
                    pending = true;
                    info!(
                        pid = instance.pid(),
                        live_mb = sample.live_mb,
                        rss_mb = sample.rss_mb,
                        "soft recycle pending, waiting for the server to settle"
                    );
                }
                continue;
            }
            Decision::Held | Decision::Stopped => continue,
            Decision::Soft => Level::Soft,
            Decision::Forced => Level::Forced,
        };

        let Some(map) = instance.map.upgrade() else {
            break;
        };
        let (live_mb, rss_mb) = instance.sample_fields();
        info!(
            pid = instance.pid(),
            ?level,
            live_mb,
            rss_mb,
            "recycling the server process"
        );
        match instance.recycle(&map).await {
            Ok(()) => {
                pending = false;
                if guard.record(level, Instant::now()) {
                    error!(
                        count = recycle::FORCED_LIMIT,
                        window = ?recycle::FORCED_WINDOW,
                        "forced recycles in a short time, not recycling this instance anymore"
                    );
                }
            }
            Err(err) => {
                error!(?err, "recycling the server process failed, closing the instance");
                instance.terminate(&map).await;
                break;
            }
        }
    }
    debug!("recycle monitor stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_in_flight_get_content_modified() {
        let keys = [
            RequestId::Number(5).tag(Tag::ClientId(1)),
            RequestId::String("abc".into()).tag(Tag::ClientId(2)),
            // not a client request
            RequestId::Number(9).tag(Tag::Drop),
            RequestId::String("garbage".into()),
        ]
        .map(|id| request_key(&id));
        let mut responses = restarted_responses(keys);
        responses.sort_by_key(|(client_id, _)| *client_id);
        assert_eq!(responses.len(), 2);

        let (client_id, error) = &responses[0];
        assert_eq!(*client_id, 1);
        assert_eq!(error.error.code, -32801);
        assert_eq!(error.error.message, "lspmux: language server restarted");
        assert!(matches!(error.id, RequestId::Number(5)));

        let (client_id, error) = &responses[1];
        assert_eq!(*client_id, 2);
        assert_eq!(error.id, "abc");
    }
}
