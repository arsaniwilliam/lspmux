//! Server settings owned by lspmux instead of the clients
//!
//! Clients sharing one server may be configured differently, or (like
//! headless agents) not at all. When `server_settings` names a settings file
//! for a server, lspmux alone answers what the server asks about its
//! configuration so it doesn't depend on which client happens to be connected.

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

/// Value at the dotted `path` (`"haskell.plugin.hlint"`) inside `value`
pub fn lookup<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| value.get(key))
}

/// Expand a leading `~/` to `$HOME`
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// Read the settings object from the JSON file at `path`
pub fn load(path: &str) -> Result<Map<String, Value>> {
    let path = expand_home(path);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("reading server settings `{}`", path.display()))?;
    let value = serde_json::from_str::<Value>(&text)
        .with_context(|| format!("parsing server settings `{}`", path.display()))?;
    match value {
        Value::Object(settings) => Ok(settings),
        _ => bail!("server settings `{}` aren't a JSON object", path.display()),
    }
}

/// Result of a `workspace/configuration` request with `params` answered from
/// `settings`
///
/// Each item's `section` is a dotted path into the settings, missing sections
/// are `null`, an item without a section gets the whole object.
pub fn configuration_result(settings: &Value, params: &Value) -> Value {
    configuration_items(params)
        .iter()
        .map(|item| match item.get("section").and_then(Value::as_str) {
            Some(section) if !section.is_empty() => {
                lookup(settings, section).cloned().unwrap_or(Value::Null)
            }
            _ => settings.clone(),
        })
        .collect()
}

/// Result of a `workspace/configuration` request nobody can answer, `null`
/// for every item
pub fn null_configuration_result(params: &Value) -> Value {
    vec![Value::Null; configuration_items(params).len()].into()
}

fn configuration_items(params: &Value) -> &[Value] {
    params["items"].as_array().map_or(&[], Vec::as_slice)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn settings() -> Value {
        json!({
            "haskell": {
                "formattingProvider": "ormolu",
                "plugin": { "hlint": { "globalOn": false } },
            },
        })
    }

    fn items(sections: &[Option<&str>]) -> Value {
        let items = sections
            .iter()
            .map(|section| match section {
                Some(section) => json!({ "section": section, "scopeUri": "file:///p/A.hs" }),
                None => json!({ "scopeUri": "file:///p/A.hs" }),
            })
            .collect::<Vec<_>>();
        json!({ "items": items })
    }

    #[test]
    fn configuration_dotted_section() {
        let result = configuration_result(
            &settings(),
            &items(&[
                Some("haskell.plugin.hlint"),
                Some("haskell.formattingProvider"),
            ]),
        );
        assert_eq!(result, json!([{ "globalOn": false }, "ormolu"]));
        let result = configuration_result(&settings(), &items(&[Some("haskell")]));
        assert_eq!(result, json!([settings()["haskell"]]));
    }

    #[test]
    fn configuration_missing_section_is_null() {
        let result = configuration_result(
            &settings(),
            &items(&[
                Some("rust-analyzer"),
                Some("haskell.plugin.missing"),
                Some("haskell.formattingProvider.x"),
            ]),
        );
        assert_eq!(result, json!([null, null, null]));
    }

    #[test]
    fn configuration_without_section_is_whole_object() {
        let result = configuration_result(&settings(), &items(&[None, Some("")]));
        assert_eq!(result, json!([settings(), settings()]));
    }

    #[test]
    fn configuration_unanswered_is_null_per_item() {
        assert_eq!(
            null_configuration_result(&items(&[None, Some("haskell")])),
            json!([null, null])
        );
        assert_eq!(null_configuration_result(&json!({})), json!([]));
    }
}
