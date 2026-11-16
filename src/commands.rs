//! Stable client-facing ids of pid-prefixed server commands
//!
//! haskell-language-server prefixes every command id with its pid
//! (`1222517:eval:evalCommand`). Clients hold on to those ids: Neovim only
//! runs commands listed in the `InitializeResult` it got on connect, code
//! lenses are cached. A recycled server has another pid, so lspmux shows the
//! clients `lspmux:<rest>` instead and puts the pid of the current server
//! process back when a message goes to the server.
//!
//! Only servers which list commands prefixed with their pid are affected.

use serde_json::Value;

/// What clients see instead of the pid
const STABLE_PREFIX: &str = "lspmux:";

fn pid_prefix(pid: u32) -> String {
    format!("{pid}:")
}

/// Does the server list commands of the form `<pid>:<rest>` in its
/// `executeCommandProvider`
pub fn uses_pid_commands(capabilities: &Value, pid: u32) -> bool {
    let prefix = pid_prefix(pid);
    capabilities["executeCommandProvider"]["commands"]
        .as_array()
        .is_some_and(|commands| {
            commands
                .iter()
                .any(|command| command.as_str().is_some_and(|c| c.starts_with(&prefix)))
        })
}

fn rewrite_prefix(string: &mut String, from: &str, to: &str) {
    if let Some(rest) = string.strip_prefix(from) {
        *string = format!("{to}{rest}");
    }
}

/// Rewrite every string value under a `command` key starting with `from`
fn rewrite_command_values(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                match value {
                    Value::String(string) if key == "command" => rewrite_prefix(string, from, to),
                    _ => rewrite_command_values(value, from, to),
                }
            }
        }
        Value::Array(array) => {
            for value in array {
                rewrite_command_values(value, from, to);
            }
        }
        _ => {}
    }
}

/// Rewrite the entries of a list of command ids
fn rewrite_command_list(commands: Option<&mut Value>, from: &str, to: &str) {
    if let Some(Value::Array(commands)) = commands {
        for command in commands {
            if let Value::String(command) = command {
                rewrite_prefix(command, from, to);
            }
        }
    }
}

/// `executeCommandProvider.commands` of the `InitializeResult` capabilities
/// for the clients
pub fn capabilities_for_clients(capabilities: &mut Value, pid: u32) {
    rewrite_command_list(
        capabilities
            .get_mut("executeCommandProvider")
            .and_then(|provider| provider.get_mut("commands")),
        &pid_prefix(pid),
        STABLE_PREFIX,
    );
}

/// A message `params` or `result` of the server of process `pid`, for the
/// clients
///
/// Besides every string under a `command` key (code lenses, code actions,
/// completion items, inlay hints, ...) this covers the `commands` of
/// `workspace/executeCommand` registrations.
pub fn to_clients(value: &mut Value, pid: u32) {
    let prefix = pid_prefix(pid);
    rewrite_command_values(value, &prefix, STABLE_PREFIX);
    if let Some(Value::Array(registrations)) = value.get_mut("registrations") {
        for registration in registrations {
            if registration["method"] == "workspace/executeCommand" {
                rewrite_command_list(
                    registration
                        .get_mut("registerOptions")
                        .and_then(|options| options.get_mut("commands")),
                    &prefix,
                    STABLE_PREFIX,
                );
            }
        }
    }
}

/// A client request `params` for the server of process `pid`
pub fn to_server(value: &mut Value, pid: u32) {
    rewrite_command_values(value, STABLE_PREFIX, &pid_prefix(pid));
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const OLD: u32 = 1222517;
    const NEW: u32 = 1300001;

    fn server_capabilities(pid: u32) -> Value {
        json!({
            "hoverProvider": true,
            "executeCommandProvider": {
                "commands": [format!("{pid}:eval:evalCommand"), format!("{pid}:hlint:applyOne"), "plain"],
            },
        })
    }

    #[test]
    fn detects_pid_prefixed_commands_of_the_current_pid_only() {
        assert!(uses_pid_commands(&server_capabilities(OLD), OLD));
        assert!(!uses_pid_commands(&server_capabilities(OLD), NEW));
        assert!(!uses_pid_commands(&json!({ "hoverProvider": true }), OLD));
        assert!(!uses_pid_commands(
            &json!({ "executeCommandProvider": { "commands": ["rust-analyzer.run"] } }),
            OLD
        ));
        assert!(!uses_pid_commands(
            &json!({ "executeCommandProvider": {} }),
            OLD
        ));
    }

    #[test]
    fn initialize_result_commands() {
        let mut capabilities = server_capabilities(OLD);
        capabilities_for_clients(&mut capabilities, OLD);
        assert_eq!(
            capabilities["executeCommandProvider"]["commands"],
            json!(["lspmux:eval:evalCommand", "lspmux:hlint:applyOne", "plain"])
        );
        assert_eq!(capabilities["hoverProvider"], true);

        // the next process lists the same ids for the clients
        let mut recycled = server_capabilities(NEW);
        capabilities_for_clients(&mut recycled, NEW);
        assert_eq!(recycled, capabilities);
    }

    #[test]
    fn server_to_clients_nested_commands() {
        let mut result = json!([
            { "range": {}, "command": { "title": "Evaluate...", "command": "1222517:eval:evalCommand", "arguments": [{ "command": "1222517:inner" }] } },
            { "title": "Fix", "kind": "quickfix", "command": { "title": "Fix", "command": "1222517:hlint:applyOne" },
              "data": { "nested": [{ "command": "1222517:deep" }] } },
        ]);
        to_clients(&mut result, OLD);
        assert_eq!(
            result,
            json!([
                { "range": {}, "command": { "title": "Evaluate...", "command": "lspmux:eval:evalCommand", "arguments": [{ "command": "lspmux:inner" }] } },
                { "title": "Fix", "kind": "quickfix", "command": { "title": "Fix", "command": "lspmux:hlint:applyOne" },
                  "data": { "nested": [{ "command": "lspmux:deep" }] } },
            ])
        );
    }

    #[test]
    fn only_command_keys_with_the_current_pid_are_touched() {
        let original = json!({
            "label": "1222517:eval:evalCommand",
            "detail": ["1222517:x"],
            "commands": ["1222517:not-a-command-key"],
            "command": { "title": "1222517:title", "command": "1300001:other-pid" },
            "other": { "command": 1222517, "command2": "1222517:y", "commandX": "1222517:z" },
            "text": "lspmux:eval:evalCommand",
        });
        let mut value = original.clone();
        to_clients(&mut value, OLD);
        assert_eq!(value, original);

        let mut value = original.clone();
        to_server(&mut value, NEW);
        assert_eq!(value, original);

        // a prefix is the whole `<pid>:`
        let mut value = json!({ "command": "12225170:x" });
        to_clients(&mut value, OLD);
        assert_eq!(value, json!({ "command": "12225170:x" }));
    }

    #[test]
    fn registrations_of_execute_command() {
        let mut params = json!({
            "registrations": [
                { "id": "1", "method": "workspace/executeCommand",
                  "registerOptions": { "commands": ["1222517:eval:evalCommand", "plain"] } },
                { "id": "2", "method": "workspace/didChangeWatchedFiles",
                  "registerOptions": { "commands": ["1222517:untouched"] } },
            ],
        });
        to_clients(&mut params, OLD);
        assert_eq!(
            params["registrations"][0]["registerOptions"]["commands"],
            json!(["lspmux:eval:evalCommand", "plain"])
        );
        assert_eq!(
            params["registrations"][1]["registerOptions"]["commands"],
            json!(["1222517:untouched"])
        );
    }

    #[test]
    fn clients_to_server_uses_the_current_pid() {
        let params = json!({
            "command": "lspmux:eval:evalCommand",
            "arguments": [{ "command": "lspmux:inner", "uri": "lspmux:not-a-command" }],
            "item": { "command": { "title": "Evaluate...", "command": "lspmux:eval:evalCommand" } },
        });

        let mut to_old = params.clone();
        to_server(&mut to_old, OLD);
        assert_eq!(to_old["command"], "1222517:eval:evalCommand");
        assert_eq!(to_old["arguments"][0]["command"], "1222517:inner");
        assert_eq!(to_old["arguments"][0]["uri"], "lspmux:not-a-command");
        assert_eq!(
            to_old["item"]["command"]["command"],
            "1222517:eval:evalCommand"
        );

        // after a recycle the same client object reaches the new pid
        let mut to_new = params.clone();
        to_server(&mut to_new, NEW);
        assert_eq!(to_new["command"], "1300001:eval:evalCommand");
        assert_eq!(to_new["arguments"][0]["command"], "1300001:inner");

        // and what the new server answers is stable again
        to_clients(&mut to_new, NEW);
        assert_eq!(to_new, params);
    }
}
