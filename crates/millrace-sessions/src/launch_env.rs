use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

pub fn current_launch_env() -> BTreeMap<String, String> {
    env::var("PATH")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| BTreeMap::from([("PATH".to_string(), value)]))
        .unwrap_or_default()
}

pub fn merge_current_launch_env(
    mut overrides: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    for (key, value) in current_launch_env() {
        overrides.entry(key).or_insert(value);
    }
    overrides
}

pub fn resolve_argv_executable(argv: &mut [String]) {
    let Some(command) = argv.first_mut() else {
        return;
    };
    let Some(path) = resolve_on_current_path(command) else {
        return;
    };
    *command = path.display().to_string();
}

fn resolve_on_current_path(command: &str) -> Option<PathBuf> {
    if command.is_empty() || command.contains('/') || command.contains('\\') {
        return None;
    }

    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_launch_env_forwards_client_path_when_present() {
        let env = current_launch_env();
        if let Ok(path) = std::env::var("PATH") {
            assert_eq!(env.get("PATH").map(String::as_str), Some(path.as_str()));
        } else {
            assert!(env.is_empty());
        }
    }
}
