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
    /// ProxyJump 中离目标最近的那一跳（别名或 user@host:port）。`ProxyJump a,b` 表示先连 a、
    /// 再经 a 连 b、最后经 b 连目标，所以目标直接经由的是 b；更前面的跳由 b 自己的设置描述。
    pub proxy_jump: Option<String>,
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
    let home = dirs::home_dir().ok_or_else(|| crate::tr!("无法定位用户主目录", "Cannot locate the home directory").to_string())?;
    Ok(home.join(".ssh").join("config"))
}

/// 展开开头的 `~`。OpenSSH 配置和用户手输的私钥路径都常这么写，而 libssh2 只认真实路径。
pub(crate) fn expand_home(path: &str) -> String {
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

/// `ProxyJump none` 表示显式不用跳板机。
fn last_jump_hop(value: &str) -> Option<String> {
    if value.eq_ignore_ascii_case("none") {
        return None;
    }
    value.rsplit(',').map(str::trim).find(|hop| !hop.is_empty()).map(str::to_string)
}

/// 一个 `Host` 块：适用的主机名模式，以及块里出现的选项（同一选项只记第一次出现的值）。
#[derive(Default)]
struct HostBlock {
    patterns: Vec<String>,
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<String>,
    proxy_jump: Option<String>,
}

/// 把一行拆成「关键字」和「值」。OpenSSH 允许 `Key Value` 和 `Key=Value` 两种写法，值可以带引号。
/// 注释只认行首的 `#` 或前面有空白的 `#`，不误伤值里本来就有的 `#`。
fn split_option(raw_line: &str) -> Option<(String, String)> {
    let mut line = raw_line.trim();
    if line.starts_with('#') {
        return None;
    }
    if let Some(comment) = line.find(" #").or_else(|| line.find("\t#")) {
        line = line[..comment].trim_end();
    }
    let key_end = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let keyword = line[..key_end].to_ascii_lowercase();
    let value = line[key_end..].trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim();
    (!keyword.is_empty() && !value.is_empty()).then(|| (keyword, value.to_string()))
}

fn unquote(value: &str) -> &str {
    value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value)
}

/// ssh_config 的通配：`*` 匹配任意多个字符，`?` 匹配一个字符。
fn pattern_matches(pattern: &str, host: &str) -> bool {
    fn matches(pattern: &[char], host: &[char]) -> bool {
        match pattern.split_first() {
            None => host.is_empty(),
            Some(('*', rest)) => (0..=host.len()).any(|skip| matches(rest, &host[skip..])),
            Some(('?', rest)) => !host.is_empty() && matches(rest, &host[1..]),
            Some((c, rest)) => host.first() == Some(c) && matches(rest, &host[1..]),
        }
    }
    matches(&pattern.chars().collect::<Vec<_>>(), &host.chars().collect::<Vec<_>>())
}

impl HostBlock {
    /// 这个块是否适用于某个主机别名：至少命中一个模式，且没有命中任何 `!` 开头的排除模式。
    fn applies_to(&self, alias: &str) -> bool {
        let (negated, positive): (Vec<&str>, Vec<&str>) =
            self.patterns.iter().map(String::as_str).partition(|p| p.starts_with('!'));
        positive.iter().any(|p| pattern_matches(p, alias))
            && !negated.iter().any(|p| pattern_matches(&p[1..], alias))
    }
}

fn is_concrete(pattern: &str) -> bool {
    !pattern.starts_with('!') && !pattern.contains('*') && !pattern.contains('?')
}

/// 解析规则与 OpenSSH 一致：从上到下读，每个选项取「第一个适用于该主机的值」。
/// 所以 `Host a b` 是两台主机，`Host *` 里的值是所有主机的缺省值，同一选项重复出现时前面的优先
/// （多个 IdentityFile 时 ssh 也是从第一个开始试）。`Match` 块的条件无法在这里求值，整块跳过。
fn parse_ssh_config(content: &str) -> Vec<SshHostEntry> {
    // 第一个 Host 之前的选项对所有主机生效，等同于放在一个 `Host *` 块里。
    let mut blocks = vec![HostBlock { patterns: vec!["*".into()], ..Default::default() }];
    let mut in_match_block = false;
    for (keyword, value) in content.lines().filter_map(split_option) {
        match keyword.as_str() {
            "host" => {
                in_match_block = false;
                let patterns = value.split_whitespace().map(|p| unquote(p).to_string()).collect();
                blocks.push(HostBlock { patterns, ..Default::default() });
            }
            "match" => in_match_block = true,
            _ if in_match_block => {}
            _ => {
                let Some(block) = blocks.last_mut() else { continue };
                let value = unquote(&value);
                match keyword.as_str() {
                    "hostname" => block.hostname.get_or_insert_with(|| value.to_string()),
                    "user" => block.user.get_or_insert_with(|| value.to_string()),
                    "identityfile" => block.identity_file.get_or_insert_with(|| expand_home(value)),
                    "proxyjump" => {
                        if block.proxy_jump.is_none() {
                            block.proxy_jump = last_jump_hop(value);
                        }
                        continue;
                    }
                    "port" => {
                        if block.port.is_none() {
                            block.port = value.parse().ok();
                        }
                        continue;
                    }
                    _ => continue,
                };
            }
        }
    }

    let mut aliases: Vec<&str> = Vec::new();
    for pattern in blocks.iter().flat_map(|block| &block.patterns) {
        if is_concrete(pattern) && !aliases.contains(&pattern.as_str()) {
            aliases.push(pattern);
        }
    }
    aliases
        .into_iter()
        .map(|alias| {
            let mut entry = SshHostEntry {
                alias: alias.to_string(),
                hostname: None,
                user: None,
                port: None,
                identity_file: None,
                proxy_jump: None,
            };
            for block in blocks.iter().filter(|block| block.applies_to(alias)) {
                entry.hostname = entry.hostname.or_else(|| block.hostname.clone());
                entry.user = entry.user.or_else(|| block.user.clone());
                entry.port = entry.port.or(block.port);
                entry.identity_file = entry.identity_file.or_else(|| block.identity_file.clone());
                entry.proxy_jump = entry.proxy_jump.or_else(|| block.proxy_jump.clone());
            }
            entry
        })
        .collect()
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
        assert_eq!(hosts[0].proxy_jump, None);
    }

    #[test]
    fn match_block_options_do_not_leak_into_previous_host() {
        let content = "Host app\n  HostName 10.0.0.5\n  User deploy\n\nMatch user root\n  User root\n  Port 2200\n";
        let hosts = parse_ssh_config(content);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].user.as_deref(), Some("deploy"));
        assert_eq!(hosts[0].port, None);
    }

    #[test]
    fn follows_openssh_rules_for_defaults_aliases_and_syntax() {
        let content = "\
User early-default
Host web1 web2 !web3
  HostName=10.0.0.%h
  IdentityFile \"~/.ssh/first key\"
  IdentityFile ~/.ssh/second
Host db # the database
  Port = 2200
Host *
  User fallback
  Port 22
";
        let hosts = parse_ssh_config(content);
        let get = |alias: &str| hosts.iter().find(|h| h.alias == alias).unwrap_or_else(|| panic!("{alias} missing"));
        assert_eq!(hosts.len(), 3, "web1, web2 and db; patterns and negations are not hosts");
        // `Host a b` 是两台主机，共用同一组选项；Key=Value 与带引号的值都能读。
        assert_eq!(get("web2").hostname.as_deref(), Some("10.0.0.%h"));
        assert!(get("web1").identity_file.as_deref().unwrap().ends_with("/.ssh/first key"), "first IdentityFile wins");
        // 文件开头的选项和 `Host *` 都是缺省值，先出现的优先。
        assert_eq!(get("web1").user.as_deref(), Some("early-default"));
        assert_eq!(get("web1").port, Some(22));
        // 行尾注释不算进值里；主机自己的设置优先于后面的 `Host *`。
        assert_eq!(get("db").port, Some(2200));
    }

    #[test]
    fn wildcard_patterns_match_like_ssh() {
        assert!(pattern_matches("*.example.com", "a.example.com"));
        assert!(pattern_matches("web?", "web1"));
        assert!(!pattern_matches("web?", "web10"));
        assert!(pattern_matches("*", "anything"));
    }

    #[test]
    fn proxy_jump_keeps_hop_nearest_to_target() {
        let content = "Host app\n  HostName 10.0.0.5\n  ProxyJump ops@gate.example.com:2222,bastion\n\
                       Host direct\n  ProxyJump none\n";
        let hosts = parse_ssh_config(content);
        assert_eq!(hosts[0].proxy_jump.as_deref(), Some("bastion"));
        assert_eq!(hosts[1].proxy_jump, None);
    }
}
