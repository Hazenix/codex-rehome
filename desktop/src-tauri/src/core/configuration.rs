use crate::core::models::CodexInventory;
use serde::{Deserialize, Serialize};
use std::{fs::File, io::Read, path::Path, process::Command};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigurationSummary {
    pub codex_version: Option<String>,
    pub settings_readable: bool,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub mcp_servers: Vec<String>,
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigurationDifference {
    pub setting: &'static str,
    pub source: Option<String>,
    pub target: Option<String>,
}

impl ConfigurationSummary {
    pub(crate) fn read(inventory: &CodexInventory) -> Self {
        let settings = read_settings(&inventory.codex_home.join("config.toml"));
        let servers = settings
            .as_ref()
            .and_then(|value| value.get("mcp_servers"))
            .and_then(toml::Value::as_table);
        let mut mcp_servers = servers
            .into_iter()
            .flat_map(|servers| servers.iter())
            .filter(|(_, settings)| {
                settings.get("enabled").and_then(toml::Value::as_bool) != Some(false)
            })
            .filter_map(|(name, _)| identifier(name))
            .collect::<Vec<_>>();
        mcp_servers.sort();
        let mut skills = inventory
            .skills
            .iter()
            .filter_map(|skill| identifier(&skill.name))
            .collect::<Vec<_>>();
        skills.sort();
        skills.dedup();

        let codex_version = crate::core::bridge::detect_registration_cli(inventory.source_os)
            .and_then(|path| Command::new(path).arg("--version").output().ok())
            .filter(|output| output.status.success())
            .and_then(|output| {
                let text = std::str::from_utf8(&output.stdout).ok()?;
                identifier(text.trim().strip_prefix("codex-cli ")?)
            });

        Self {
            codex_version,
            settings_readable: settings.is_some(),
            model: setting_identifier(settings.as_ref(), "model"),
            model_provider: setting_identifier(settings.as_ref(), "model_provider"),
            mcp_servers,
            skills,
        }
    }

    pub(crate) fn differences_from(&self, target: &Self) -> Vec<ConfigurationDifference> {
        let mut different = Vec::new();
        let mut compare = |setting, source: Option<String>, target: Option<String>| {
            if source != target {
                different.push(ConfigurationDifference {
                    setting,
                    source,
                    target,
                });
            }
        };
        compare(
            "codex_version",
            self.codex_version.clone(),
            target.codex_version.clone(),
        );
        compare(
            "model",
            describe_setting(self.settings_readable, self.model.as_deref()),
            describe_setting(target.settings_readable, target.model.as_deref()),
        );
        compare(
            "model_provider",
            describe_setting(self.settings_readable, self.model_provider.as_deref()),
            describe_setting(target.settings_readable, target.model_provider.as_deref()),
        );
        compare(
            "mcp_servers",
            self.settings_readable.then(|| self.mcp_servers.join(", ")),
            target
                .settings_readable
                .then(|| target.mcp_servers.join(", ")),
        );
        compare(
            "skills",
            Some(self.skills.join(", ")),
            Some(target.skills.join(", ")),
        );
        different
    }
}

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

fn read_settings(path: &Path) -> Option<toml::Value> {
    let mut text = String::new();
    match File::open(path) {
        Ok(file) => file
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut text)
            .ok()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(_) => return None,
    };
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return None;
    }
    toml::from_str(&text).ok()
}

fn setting_identifier(settings: Option<&toml::Value>, key: &str) -> Option<String> {
    identifier(settings?.get(key)?.as_str()?)
}

fn describe_setting(readable: bool, value: Option<&str>) -> Option<String> {
    // None 表示无法读取，空字符串表示没有设置该项。
    readable.then(|| value.unwrap_or_default().to_owned())
}

fn identifier(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+'))
        && !value.starts_with("sk-"))
    .then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::discovery::discover_codex;

    #[test]
    fn compares_only_config_identifiers_without_serializing_secrets() {
        let source_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            source_dir.path().join("config.toml"),
            "model = 'test-model'\nmodel_provider = 'example'\n[mcp_servers.docs]\nenabled = true\nurl = 'https://example.invalid?secret=never-export-this'\n[mcp_servers.disabled]\nenabled = false\n[model_providers.example]\napi_key = 'never-export-this'\n",
        )
        .unwrap();
        let source =
            ConfigurationSummary::read(&discover_codex(Some(source_dir.path().into())).unwrap());
        let target =
            ConfigurationSummary::read(&discover_codex(Some(target_dir.path().into())).unwrap());
        assert_eq!(source.mcp_servers, ["docs"]);
        let json = serde_json::to_string(&source).unwrap();
        assert!(!json.contains("never-export-this"));
        assert!(source.differences_from(&target).iter().any(|difference| {
            difference.setting == "model_provider"
                && difference.source.as_deref() == Some("example")
                && difference.target.as_deref() == Some("")
        }));
    }
}
