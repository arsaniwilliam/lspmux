//! Server work done progress and quiet time
//!
//! Tracks the server's active progress tokens so clients attaching to a
//! running server see what it's working on, and when the server last showed
//! activity so lspmux can tell when it settled.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::lsp::ext::Tag;
use crate::lsp::jsonrpc::{Message, Notification, Request, RequestId, Version};

/// Progress token lspmux owns, announces attaching to a server with nothing
/// in progress
pub const ATTACH_TOKEN: &str = "lspmux/attach";

/// Title of the attach announcement
const ATTACH_TITLE: &str = "lspmux: attached to running server";

struct Token {
    token: Value,
    /// Created by the server with `window/workDoneProgress/create`
    created: bool,
    begin: Option<Value>,
    report: Option<Value>,
}

/// How long a freshly started server may stay silent before it counts as
/// settled anyway
const STARTUP_GRACE: Duration = Duration::from_secs(30);

pub struct Progress {
    /// Not ended yet, in creation order
    tokens: Vec<Token>,
    /// Latest server `publishDiagnostics` or `$/progress`, or document sync
    /// lspmux sent the server
    last_activity: Instant,
    /// A client attached to the instance before
    attached: bool,
    /// Since when a restarted server wasn't heard from, it isn't settled
    /// before it showed any activity (or [`STARTUP_GRACE`] passed): it's
    /// loading, not done
    silent_since: Option<Instant>,
}

impl Progress {
    pub fn new(now: Instant) -> Progress {
        Progress {
            tokens: Vec::new(),
            last_activity: now,
            attached: false,
            silent_since: None,
        }
    }

    /// Activity of lspmux towards the server, e.g. document sync
    pub fn touch(&mut self, now: Instant) {
        self.last_activity = self.last_activity.max(now);
    }

    /// Activity of the server: `publishDiagnostics` or `$/progress`
    pub fn server_activity(&mut self, now: Instant) {
        self.touch(now);
        self.silent_since = None;
    }

    /// The server process is being replaced: its tokens are gone, returns
    /// the `$/progress` end messages for the clients which saw them begin
    pub fn end_all(&mut self) -> Vec<Message> {
        self.tokens
            .drain(..)
            .filter(|token| token.begin.is_some())
            .map(|token| progress(&token.token, json!({ "kind": "end" })))
            .collect()
    }

    /// The replacement server process is up
    pub fn restarted(&mut self, now: Instant) {
        self.last_activity = now;
        self.silent_since = Some(now);
    }

    fn token_mut(&mut self, token: &Value) -> Option<&mut Token> {
        self.tokens.iter_mut().find(|t| t.token == *token)
    }

    /// Handle `window/workDoneProgress/create` server request `params`
    pub fn create(&mut self, params: &Value) {
        let token = &params["token"];
        if token.is_null() || self.token_mut(token).is_some() {
            return;
        }
        self.tokens.push(Token {
            token: token.clone(),
            created: true,
            begin: None,
            report: None,
        });
    }

    /// Handle `$/progress` server notification `params`
    pub fn progress(&mut self, params: &Value, now: Instant) {
        self.server_activity(now);
        let token = &params["token"];
        let value = &params["value"];
        match value["kind"].as_str() {
            Some("begin") => match self.token_mut(token) {
                Some(entry) => {
                    entry.begin = Some(value.clone());
                    entry.report = None;
                }
                // Not created by the server, e.g. a client request's token
                None => self.tokens.push(Token {
                    token: token.clone(),
                    created: false,
                    begin: Some(value.clone()),
                    report: None,
                }),
            },
            Some("report") => {
                if let Some(entry) = self.token_mut(token) {
                    entry.report = Some(value.clone());
                }
            }
            Some("end") => self.tokens.retain(|t| t.token != *token),
            _ => {}
        }
    }

    fn any_active(&self) -> bool {
        self.tokens.iter().any(|t| t.begin.is_some())
    }

    /// No progress active and nothing happened for at least `settle`
    pub fn settled(&self, now: Instant, settle: Duration) -> bool {
        if self
            .silent_since
            .is_some_and(|since| now.saturating_duration_since(since) < STARTUP_GRACE)
        {
            return false;
        }
        !self.any_active() && now.saturating_duration_since(self.last_activity) >= settle
    }

    /// Messages for a client attaching to the instance
    ///
    /// Clients declaring `window.workDoneProgress` get every active token
    /// replayed. With nothing active they get a progress which ends right
    /// away, for clients waiting for the server to finish loading, except the
    /// first client of the instance which sees the server's own startup
    /// progress.
    pub fn attach(&mut self, work_done_progress: bool) -> Vec<Message> {
        let first = !self.attached;
        self.attached = true;
        if !work_done_progress {
            return Vec::new();
        }
        let mut messages = Vec::new();
        for token in &self.tokens {
            if token.created {
                messages.push(create(&token.token));
            }
            for value in [&token.begin, &token.report].into_iter().flatten() {
                messages.push(progress(&token.token, value.clone()));
            }
        }
        if !self.any_active() && !first {
            let token = Value::from(ATTACH_TOKEN);
            messages.extend([
                create(&token),
                progress(&token, json!({ "kind": "begin", "title": ATTACH_TITLE })),
                progress(&token, json!({ "kind": "end" })),
            ]);
        }
        messages
    }
}

fn create(token: &Value) -> Message {
    // The client's response is dropped
    let id = RequestId::String(format!("replay:workDoneProgress/create:{token}")).tag(Tag::Drop);
    Request {
        jsonrpc: Version,
        method: "window/workDoneProgress/create".into(),
        params: json!({ "token": token }),
        id,
    }
    .into()
}

fn progress(token: &Value, value: Value) -> Message {
    Notification {
        jsonrpc: Version,
        method: "$/progress".into(),
        params: json!({ "token": token, "value": value }),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLE: Duration = Duration::from_millis(1500);

    fn begin(token: Value, title: &str) -> Value {
        json!({ "token": token, "value": { "kind": "begin", "title": title } })
    }

    fn report(token: Value, message: &str) -> Value {
        json!({ "token": token, "value": { "kind": "report", "message": message } })
    }

    fn end(token: Value) -> Value {
        json!({ "token": token, "value": { "kind": "end" } })
    }

    /// `(method, params)` of each message
    fn summary(messages: &[Message]) -> Vec<(String, Value)> {
        messages
            .iter()
            .map(|message| match message {
                Message::Request(req) => (req.method.clone(), req.params.clone()),
                Message::Notification(notif) => (notif.method.clone(), notif.params.clone()),
                _ => panic!("unexpected response"),
            })
            .collect()
    }

    #[test]
    fn settles_after_quiet_time_without_progress() {
        let start = Instant::now();
        let mut progress = Progress::new(start);
        assert!(!progress.settled(start, SETTLE));
        assert!(progress.settled(start + SETTLE, SETTLE));

        // activity restarts the quiet time
        progress.touch(start + Duration::from_secs(1));
        assert!(!progress.settled(start + SETTLE, SETTLE));
        assert!(progress.settled(start + Duration::from_secs(1) + SETTLE, SETTLE));

        // an active token keeps it busy however long it takes
        let later = start + Duration::from_secs(10);
        progress.create(&json!({ "token": 1 }));
        progress.progress(&begin(json!(1), "Processing"), later);
        assert!(!progress.settled(later + Duration::from_secs(60), SETTLE));
        progress.progress(&end(json!(1)), later + Duration::from_secs(60));
        assert!(!progress.settled(later + Duration::from_secs(61), SETTLE));
        assert!(progress.settled(later + Duration::from_secs(60) + SETTLE, SETTLE));
    }

    #[test]
    fn created_token_without_begin_is_not_active() {
        let start = Instant::now();
        let mut progress = Progress::new(start);
        progress.create(&json!({ "token": "t" }));
        assert!(progress.settled(start + SETTLE, SETTLE));
    }

    #[test]
    fn replay_of_active_tokens() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        // first client of the instance
        assert!(progress.attach(true).is_empty());

        progress.create(&json!({ "token": "setup" }));
        progress.progress(&begin(json!("setup"), "Setting up p"), now);
        progress.progress(&report(json!("setup"), "1/3"), now);
        progress.progress(&report(json!("setup"), "2/3"), now);
        // a client request's token, not created by the server
        progress.progress(&begin(json!(7), "Indexing"), now);
        progress.create(&json!({ "token": "gone" }));
        progress.progress(&begin(json!("gone"), "Processing"), now);
        progress.progress(&end(json!("gone")), now);

        let messages = progress.attach(true);
        assert_eq!(
            summary(&messages),
            [
                (
                    "window/workDoneProgress/create".into(),
                    json!({ "token": "setup" })
                ),
                ("$/progress".into(), begin(json!("setup"), "Setting up p")),
                ("$/progress".into(), report(json!("setup"), "2/3")),
                ("$/progress".into(), begin(json!(7), "Indexing")),
            ]
        );
        // the client's responses to replayed requests are dropped
        let Message::Request(req) = &messages[0] else {
            unreachable!()
        };
        assert!(matches!(req.id.untag(), (Some(Tag::Drop), _)));

        // clients without work done progress support get nothing
        assert!(progress.attach(false).is_empty());
    }

    #[test]
    fn attach_announcement_only_when_idle_and_not_first() {
        let now = Instant::now();
        let mut progress = Progress::new(now);
        // the first client sees the server's own startup progress
        assert!(progress.attach(true).is_empty());

        let announcement = [
            (
                "window/workDoneProgress/create".into(),
                json!({ "token": ATTACH_TOKEN }),
            ),
            (
                "$/progress".into(),
                begin(json!(ATTACH_TOKEN), ATTACH_TITLE),
            ),
            ("$/progress".into(), end(json!(ATTACH_TOKEN))),
        ];
        assert_eq!(summary(&progress.attach(true)), announcement);

        // not while the server is busy
        progress.create(&json!({ "token": 1 }));
        progress.progress(&begin(json!(1), "Processing"), now);
        let messages = summary(&progress.attach(true));
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|(_, params)| params["token"] == 1));

        progress.progress(&end(json!(1)), now);
        assert_eq!(summary(&progress.attach(true)), announcement);

        // a first client without progress support still uses up the exemption
        let mut progress = Progress::new(now);
        assert!(progress.attach(false).is_empty());
        assert_eq!(summary(&progress.attach(true)), announcement);
    }

    #[test]
    fn restart_ends_begun_tokens_and_waits_for_the_new_server() {
        let start = Instant::now();
        let mut progress = Progress::new(start);
        progress.create(&json!({ "token": "a" }));
        progress.progress(&begin(json!("a"), "Indexing"), start);
        progress.create(&json!({ "token": "created-only" }));
        progress.progress(&begin(json!(7), "Processing"), start);

        let restart = start + Duration::from_secs(100);
        let ends = summary(&progress.end_all());
        progress.restarted(restart);
        assert_eq!(
            ends,
            [
                ("$/progress".into(), end(json!("a"))),
                ("$/progress".into(), end(json!(7))),
            ]
        );
        // nothing left to replay
        assert!(progress.attach(true).len() <= 3);

        // quiet isn't enough while the new server hasn't said anything
        assert!(!progress.settled(restart + Duration::from_secs(10), SETTLE));
        // its first message makes the quiet time count again
        let heard = restart + Duration::from_secs(5);
        progress.server_activity(heard);
        assert!(!progress.settled(heard, SETTLE));
        assert!(progress.settled(heard + SETTLE, SETTLE));

        // a server which never says anything settles after the grace time
        let mut progress = Progress::new(start);
        progress.restarted(restart);
        assert!(!progress.settled(restart + STARTUP_GRACE - Duration::from_millis(1), SETTLE));
        assert!(progress.settled(restart + STARTUP_GRACE, SETTLE));
    }
}
