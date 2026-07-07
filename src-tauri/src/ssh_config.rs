use serde::{Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct SshHostEntry {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
}

/// 读取 ~/.ssh/config，方便用户直接选已有 Host 而不用手填。
pub fn list_ssh_hosts() -> Result<Vec<SshHostEntry>, String> {
    let config_path = ssh_config_path()?;
    if !config_path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    Ok(parse_ssh_config(&content))
}

pub fn resolve_host(alias: &str) -> Result<Option<SshHostEntry>, String> {
    let hosts = list_ssh_hosts()?;
    Ok(hosts.into_iter().find(|h| h.alias == alias))
}

fn ssh_config_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "无法定位用户主目录".to_string())?;
    Ok(home.join(".ssh").join("config"))
}

fn expand_path(path: &str) -> String {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped).to_string_lossy().to_string();
        }
    }
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().to_string();
        }
    }
    path.to_string()
}

fn parse_ssh_config(content: &str) -> Vec<SshHostEntry> {
    let mut entries = Vec::new();
    let mut current: Option<SshHostEntry> = None;

    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.split_whitespace();
        let keyword = parts.next().unwrap_or("").to_ascii_lowercase();

        match keyword.as_str() {
            "host" => {
                if let Some(entry) = current.take() {
                    if !entry.alias.contains('*') && !entry.alias.contains('?') {
                        entries.push(entry);
                    }
                }

                let pattern = parts.next().unwrap_or("").to_string();
                if pattern == "*" {
                    current = None;
                    continue;
                }

                current = Some(SshHostEntry {
                    alias: pattern,
                    hostname: None,
                    user: None,
                    port: None,
                    identity_file: None,
                });
            }
            "hostname" if current.is_some() => {
                if let Some(value) = parts.next() {
                    current.as_mut().unwrap().hostname = Some(value.to_string());
                }
            }
            "user" if current.is_some() => {
                if let Some(value) = parts.next() {
                    current.as_mut().unwrap().user = Some(value.to_string());
                }
            }
            "port" if current.is_some() => {
                if let Some(value) = parts.next() {
                    if let Ok(port) = value.parse::<u16>() {
                        current.as_mut().unwrap().port = Some(port);
                    }
                }
            }
            "identityfile" if current.is_some() => {
                if let Some(value) = parts.next() {
                    current.as_mut().unwrap().identity_file = Some(expand_path(value));
                }
            }
            _ => {}
        }
    }

    if let Some(entry) = current {
        if !entry.alias.contains('*') && !entry.alias.contains('?') {
            entries.push(entry);
        }
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_host_block() {
        let content = r#"
Host myserver
    HostName 192.168.1.10
    User deploy
    Port 2222
    IdentityFile ~/.ssh/id_ed25519
"#;
        let hosts = parse_ssh_config(content);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "myserver");
        assert_eq!(hosts[0].hostname.as_deref(), Some("192.168.1.10"));
        assert_eq!(hosts[0].user.as_deref(), Some("deploy"));
        assert_eq!(hosts[0].port, Some(2222));
        assert!(hosts[0].identity_file.as_ref().unwrap().ends_with(".ssh/id_ed25519"));
    }
}
