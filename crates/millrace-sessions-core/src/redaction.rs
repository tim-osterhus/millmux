use std::collections::{BTreeMap, BTreeSet};

pub const REDACTED_VALUE: &str = "[redacted]";

const SECRET_KEY_PARTS: [&str; 5] = ["TOKEN", "SECRET", "PASSWORD", "KEY", "AUTH"];

pub fn redact_env(
    env: &BTreeMap<String, String>,
    allowlist: impl IntoIterator<Item = impl AsRef<str>>,
) -> BTreeMap<String, String> {
    let allowed = allowlist
        .into_iter()
        .map(|key| key.as_ref().to_string())
        .collect::<BTreeSet<_>>();

    env.iter()
        .filter(|(key, _)| allowed.contains(*key))
        .map(|(key, value)| {
            let persisted = if is_likely_secret_key(key) {
                REDACTED_VALUE.to_string()
            } else {
                value.clone()
            };
            (key.clone(), persisted)
        })
        .collect()
}

pub fn is_likely_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SECRET_KEY_PARTS.iter().any(|part| upper.contains(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_is_allowlist_only() {
        let env = BTreeMap::from([
            ("PATH".to_string(), "/bin".to_string()),
            ("HOME".to_string(), "/home/me".to_string()),
        ]);
        let redacted = redact_env(&env, ["PATH"]);
        assert_eq!(redacted.len(), 1);
        assert_eq!(redacted.get("PATH").unwrap(), "/bin");
    }

    #[test]
    fn redaction_masks_secret_like_keys() {
        let env = BTreeMap::from([
            ("API_TOKEN".to_string(), "token-value".to_string()),
            ("monkey_patch".to_string(), "yes".to_string()),
            ("AUTH_MODE".to_string(), "local".to_string()),
            ("VISIBLE".to_string(), "ok".to_string()),
        ]);
        let redacted = redact_env(&env, ["API_TOKEN", "monkey_patch", "AUTH_MODE", "VISIBLE"]);
        assert_eq!(redacted.get("API_TOKEN").unwrap(), REDACTED_VALUE);
        assert_eq!(redacted.get("monkey_patch").unwrap(), REDACTED_VALUE);
        assert_eq!(redacted.get("AUTH_MODE").unwrap(), REDACTED_VALUE);
        assert_eq!(redacted.get("VISIBLE").unwrap(), "ok");
    }
}
