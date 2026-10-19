//! `--api-json` / `CREATE_EXTRA_JSON`: extra fields for a provider's create request.
//!
//! The provider APIs take more than the tool has flags for — RunPod v2's `dataCenterIds`,
//! `globalNetworking`, a network-volume mount (`mounts.network`), `gpu.minRamPerGpu`; v1's
//! `interruptible`; Vast's `price` bid; Hetzner's `labels` and `user_data`. Rather than a
//! flag per option, `pods create`/`up` take one JSON object (and config `CREATE_EXTRA_JSON`
//! one more, under it) that is **deep-merged into the create body** the backend builds.
//!
//! What the tool sets itself is **refused**, before any request: the name, image, GPU,
//! tier, disk, the SSH keys, the ports' `22/tcp`, the start command — each backend lists
//! its own ([`Managed`]). Those have flags or config keys (the refusal names them), and
//! the rest of the tool depends on them: the name is how every command finds the pod, the
//! keys and `22/tcp` are how setup reaches it, the volume path is what the restart gate
//! reads. Everything else passes through unchecked — an option the API rejects comes back
//! as the API's own error (a RunPod v2 422 is a config error, never retried).
//!
//! Merge rule ([`deep_merge`]): objects merge key by key, recursively (so `{"env":
//! {"FOO": "1"}}` adds one variable and keeps ours); anything else — arrays, scalars,
//! `null` — replaces the value (so `ports` is the whole list).

use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// The extra fields: always a JSON object.
pub type Extra = Map<String, Value>;

/// Where the extra JSON comes from, for messages.
pub const SOURCES: &str = "--api-json / CREATE_EXTRA_JSON";

/// Parse one source (`what` names it in the error). It must be a JSON object. The error
/// gives serde's reason and position but never echoes the text: it may carry a token.
pub fn parse(text: &str, what: &str) -> Result<Extra> {
    let v: Value = serde_json::from_str(text.trim())
        .map_err(|e| Error::Config(format!("{what}: not valid JSON ({e}) — pass one object, e.g. '{{\"dataCenterIds\": [\"EU-RO-1\"]}}'")))?;
    match v {
        Value::Object(m) => Ok(m),
        other => Err(Error::Config(format!(
            "{what}: must be a JSON object (`{{…}}`), got {} — e.g. '{{\"dataCenterIds\": [\"EU-RO-1\"]}}'",
            kind(&other)
        ))),
    }
}

/// The extra fields for a create: config `CREATE_EXTRA_JSON` first, the `--api-json` flag
/// deep-merged over it (the flag wins on a conflict). `None` when neither is set (or both
/// are blank). Pure: parsed before anything is listed or created.
pub fn combine(config: Option<&str>, flag: Option<&str>) -> Result<Option<Extra>> {
    let mut out: Option<Value> = None;
    for (text, what) in [(config, "CREATE_EXTRA_JSON"), (flag, "--api-json")] {
        let Some(text) = text.filter(|t| !t.trim().is_empty()) else { continue };
        let extra = Value::Object(parse(text, what)?);
        match &mut out {
            None => out = Some(extra),
            Some(base) => deep_merge(base, &extra),
        }
    }
    Ok(out.map(|v| match v {
        Value::Object(m) => m,
        _ => unreachable!("only objects are merged"),
    }))
}

/// Merge `extra` into `body`: where both are objects, key by key (recursively); otherwise
/// `extra` replaces `body` — arrays and scalars are taken whole, and so is `null`.
pub fn deep_merge(body: &mut Value, extra: &Value) {
    match (body, extra) {
        (Value::Object(b), Value::Object(x)) => {
            for (k, v) in x {
                match b.get_mut(k) {
                    Some(slot) => deep_merge(slot, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, v) => *slot = v.clone(),
    }
}

/// A field of the create body the tool sets itself. `path` is its location (`["gpu",
/// "id"]` = `gpu.id`); `by` says what sets it — the flag/config key the operator should use.
#[derive(Debug, Clone, Copy)]
pub struct Managed {
    pub path: &'static [&'static str],
    pub by: &'static str,
}

/// Shorthand for the backends' tables.
pub const fn managed(path: &'static [&'static str], by: &'static str) -> Managed {
    Managed { path, by }
}

/// Whether `extra` would set `path`: the path itself is present — or one of its parents is
/// given as something that isn't an object, which would replace the whole subtree the
/// managed field lives in (`{"env": null}` drops `env.PUBLIC_KEY`).
fn touches(extra: &Extra, path: &[&str]) -> bool {
    let mut cur = extra;
    for (i, key) in path.iter().enumerate() {
        match cur.get(*key) {
            None => return false,
            Some(_) if i + 1 == path.len() => return true,
            Some(Value::Object(next)) => cur = next,
            Some(_) => return true,
        }
    }
    false
}

/// The ports rule: a `ports` list given in `extra` replaces ours whole, so it must still
/// expose SSH (`22/tcp`) — every command reaches the pod over it. `key` is the backend's
/// field (`ports`); `None` = this backend has no ports list.
fn check_ports(extra: &Extra, key: &str) -> Option<String> {
    let v = extra.get(key)?;
    let ports = match v.as_array() {
        Some(list) if list.iter().all(Value::is_string) => list,
        _ => return Some(format!("`{key}` must be a list of strings like [\"8888/http\", \"22/tcp\"] (it replaces ours whole)")),
    };
    let ssh = ports.iter().filter_map(Value::as_str).any(|p| p.trim().eq_ignore_ascii_case("22/tcp"));
    (!ssh).then(|| {
        format!("`{key}` must keep \"22/tcp\" — it replaces our list whole, and every command reaches the pod over SSH")
    })
}

/// Refuse `extra` if it sets a field the tool manages, or drops `22/tcp` from `ports_key`'s
/// list. Every violation is named at once, with what sets that field instead. Pure.
pub fn check(extra: &Extra, managed: &[Managed], ports_key: Option<&str>) -> Result<()> {
    let mut why: Vec<String> = managed
        .iter()
        .filter(|m| touches(extra, m.path))
        .map(|m| format!("`{}` is set by arena ({})", m.path.join("."), m.by))
        .collect();
    if let Some(problem) = ports_key.and_then(|k| check_ports(extra, k)) {
        why.push(problem);
    }
    if why.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(format!("{SOURCES}: refused — {}", why.join("; "))))
    }
}

/// Check `extra` ([`check`]) and merge it into a backend's create `body`. `None` leaves the
/// body as built. What every backend's create-body builder calls last.
pub fn apply(body: &mut Value, extra: Option<&Extra>, managed: &[Managed], ports_key: Option<&str>) -> Result<()> {
    if let Some(extra) = extra {
        check(extra, managed, ports_key)?;
        deep_merge(body, &Value::Object(extra.clone()));
    }
    Ok(())
}

/// What a JSON value is, for messages (never its content).
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Object(_) => "an object",
        Value::Array(_) => "an array",
        Value::String(_) => "a string",
        Value::Number(_) => "a number",
        Value::Bool(_) => "a bool",
        Value::Null => "null",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Extra {
        v.as_object().unwrap().clone()
    }

    /// (case, body, extra, merged)
    #[test]
    fn deep_merge_table() {
        let cases = [
            ("adds a key", json!({"a": 1}), json!({"b": 2}), json!({"a": 1, "b": 2})),
            ("replaces a scalar", json!({"a": 1}), json!({"a": "x"}), json!({"a": "x"})),
            ("merges nested objects", json!({"env": {"A": "1"}}), json!({"env": {"B": "2"}}), json!({"env": {"A": "1", "B": "2"}})),
            ("arrays replace whole", json!({"ports": ["22/tcp"]}), json!({"ports": ["22/tcp", "6006/http"]}), json!({"ports": ["22/tcp", "6006/http"]})),
            ("null replaces (not delete)", json!({"a": {"b": 1}}), json!({"a": null}), json!({"a": null})),
            ("object over scalar", json!({"a": 1}), json!({"a": {"b": 2}}), json!({"a": {"b": 2}})),
            ("deep", json!({"gpu": {"id": "x", "count": 1}}), json!({"gpu": {"minRamPerGpu": 32}}), json!({"gpu": {"id": "x", "count": 1, "minRamPerGpu": 32}})),
        ];
        for (case, mut body, extra, want) in cases {
            deep_merge(&mut body, &extra);
            assert_eq!(body, want, "{case}");
        }
    }

    #[test]
    fn parse_wants_one_object_and_never_echoes_the_text() {
        assert_eq!(parse(r#" {"dataCenterIds": ["EU-RO-1"]} "#, "--api-json").unwrap(), obj(json!({"dataCenterIds": ["EU-RO-1"]})));
        for (text, want) in [
            (r#"{"a": "hf_SECRET"#, "not valid JSON"),
            ("[1, 2]", "must be a JSON object (`{…}`), got an array"),
            (r#""hf_SECRET""#, "got a string"),
            ("null", "got null"),
            ("", "not valid JSON"),
        ] {
            let e = parse(text, "--api-json").unwrap_err();
            assert!(matches!(e, Error::Config(_)), "{text}: {e:?}");
            let msg = e.to_string();
            assert!(msg.contains("--api-json") && msg.contains(want), "{text}: {msg}");
            assert!(!msg.contains("SECRET"), "{text}: echoed: {msg}");
        }
    }

    /// The flag wins over the config, merged deep; blanks are nothing; either one invalid
    /// fails (naming which).
    #[test]
    fn combine_layers_the_flag_over_the_config() {
        assert_eq!(combine(None, None).unwrap(), None);
        assert_eq!(combine(Some(""), Some("  ")).unwrap(), None);
        let cfg = r#"{"dataCenterIds": ["EU-RO-1"], "env": {"A": "cfg"}, "globalNetworking": true}"#;
        let flag = r#"{"dataCenterIds": ["US-KS-2"], "env": {"B": "flag"}}"#;
        assert_eq!(
            combine(Some(cfg), Some(flag)).unwrap().unwrap(),
            obj(json!({"dataCenterIds": ["US-KS-2"], "env": {"A": "cfg", "B": "flag"}, "globalNetworking": true}))
        );
        assert_eq!(combine(Some(cfg), None).unwrap().unwrap()["globalNetworking"], true);
        let e = combine(Some("[]"), Some(flag)).unwrap_err().to_string();
        assert!(e.contains("CREATE_EXTRA_JSON"), "{e}");
        let e = combine(Some(cfg), Some("{")).unwrap_err().to_string();
        assert!(e.contains("--api-json"), "{e}");
    }

    const RULES: &[Managed] = &[
        managed(&["name"], "the machine name"),
        managed(&["gpu", "id"], "--gpu / GPU_TYPE"),
        managed(&["env", "PUBLIC_KEY"], "the cohort SSH keys"),
    ];

    /// (case, extra, Ok?, message fragments)
    #[test]
    fn check_refuses_managed_fields_and_a_port_list_without_ssh() {
        let cases: Vec<(&str, Value, bool, Vec<&str>)> = vec![
            ("unmanaged top-level", json!({"dataCenterIds": ["EU-RO-1"], "globalNetworking": true}), true, vec![]),
            ("unmanaged sibling of a managed field", json!({"gpu": {"minRamPerGpu": 32}, "env": {"FOO": "1"}}), true, vec![]),
            ("ports keeping ssh", json!({"ports": ["8888/http", " 22/tcp ", "6006/http"]}), true, vec![]),
            ("a managed top-level field", json!({"name": "x"}), false, vec!["`name` is set by arena (the machine name)"]),
            ("a managed nested field", json!({"gpu": {"id": "H100"}}), false, vec!["`gpu.id` is set by arena (--gpu / GPU_TYPE)"]),
            ("a parent replaced by a scalar", json!({"gpu": "H100"}), false, vec!["`gpu.id`"]),
            ("a parent replaced by null", json!({"env": null}), false, vec!["`env.PUBLIC_KEY`"]),
            ("ports without ssh", json!({"ports": ["8888/http"]}), false, vec!["must keep \"22/tcp\""]),
            ("ports as a string", json!({"ports": "22/tcp"}), false, vec!["must be a list of strings"]),
            ("ports with a non-string", json!({"ports": ["22/tcp", 8888]}), false, vec!["must be a list of strings"]),
            (
                "every violation at once",
                json!({"name": "x", "env": {"PUBLIC_KEY": "k", "FOO": "1"}, "ports": []}),
                false,
                vec!["`name`", "`env.PUBLIC_KEY`", "22/tcp"],
            ),
        ];
        for (case, extra, ok, frags) in cases {
            let r = check(&obj(extra), RULES, Some("ports"));
            assert_eq!(r.is_ok(), ok, "{case}: {r:?}");
            if let Err(e) = r {
                assert!(matches!(e, Error::Config(_)), "{case}");
                let msg = e.to_string();
                assert!(msg.contains(SOURCES), "{case}: {msg}");
                for f in frags {
                    assert!(msg.contains(f), "{case}: `{f}` in {msg}");
                }
            }
        }
        // No ports rule on a backend without a ports list: anything goes there.
        assert!(check(&obj(json!({"ports": []})), RULES, None).is_ok());
    }

    #[test]
    fn apply_checks_then_merges() {
        let mut body = json!({"name": "devtest-a", "gpu": {"id": "A4000", "count": 1}, "env": {"PUBLIC_KEY": "k"}});
        apply(&mut body, None, RULES, None).unwrap();
        assert_eq!(body["gpu"], json!({"id": "A4000", "count": 1}), "None leaves the body alone");
        apply(&mut body, Some(&obj(json!({"gpu": {"minRamPerGpu": 8}, "env": {"FOO": "1"}}))), RULES, None).unwrap();
        assert_eq!(body["gpu"], json!({"id": "A4000", "count": 1, "minRamPerGpu": 8}));
        assert_eq!(body["env"], json!({"PUBLIC_KEY": "k", "FOO": "1"}));
        let before = body.clone();
        assert!(apply(&mut body, Some(&obj(json!({"name": "other", "x": 1}))), RULES, None).is_err());
        assert_eq!(body, before, "a refused extra changes nothing");
    }
}
