//! Minimal parser for `~/.ssh/config` host blocks.

use std::path::{Path, PathBuf};

use sqlly_core::connection::{SshAuth, SshHop};

/// OpenSSH's own nesting limit for `Include`; also stops include cycles.
const MAX_INCLUDE_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshConfigHost {
    pub alias: String,
    pub host_name: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub identity_file: Option<String>,
}

/// Parse OpenSSH config text into discrete host blocks.
pub fn parse_ssh_config(text: &str) -> Vec<SshConfigHost> {
    parse_ssh_config_in(text, dirs::home_dir().as_deref())
}

fn parse_ssh_config_in(text: &str, home: Option<&Path>) -> Vec<SshConfigHost> {
    let mut hosts = Vec::new();
    let mut current: Option<SshConfigHost> = None;
    let mut in_match = false;

    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (keyword, value) = match line.split_once([' ', '\t', '=']) {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        let keyword = keyword.to_ascii_lowercase();
        if keyword == "match" {
            in_match = true;
            continue;
        }
        if keyword == "host" {
            in_match = false;
            if let Some(host) = current.take() {
                hosts.push(host);
            }
            if should_skip_host(value) {
                current = None;
            } else {
                current = Some(SshConfigHost {
                    alias: value.to_string(),
                    host_name: None,
                    port: None,
                    user: None,
                    identity_file: None,
                });
            }
            continue;
        }
        if in_match {
            continue;
        }
        let Some(host) = current.as_mut() else {
            continue;
        };
        match keyword.as_str() {
            "hostname" if host.host_name.is_none() => host.host_name = Some(value.to_string()),
            "port" if host.port.is_none() => {
                if let Ok(port) = value.parse::<u16>() {
                    host.port = Some(port);
                }
            }
            "user" if host.user.is_none() => host.user = Some(value.to_string()),
            "identityfile" if host.identity_file.is_none() => {
                host.identity_file = Some(expand_tilde(value, home));
            }
            _ => {}
        }
    }
    if let Some(host) = current {
        hosts.push(host);
    }
    hosts
}

fn should_skip_host(value: &str) -> bool {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 1 {
        return true;
    }
    let token = tokens[0];
    token.contains('*') || token.contains('?') || token.contains('!')
}

fn expand_tilde(path: &str, home: Option<&Path>) -> String {
    match (path.strip_prefix("~/"), home) {
        // Join each segment so the result uses the platform's separator
        // (`~/.ssh/key` becomes `C:\Users\me\.ssh\key` on Windows).
        (Some(rest), Some(home)) => rest
            .split('/')
            .filter(|part| !part.is_empty())
            .fold(home.to_path_buf(), |path, part| path.join(part))
            .display()
            .to_string(),
        _ if path == "~" => home
            .map(|home| home.display().to_string())
            .unwrap_or_else(|| path.to_string()),
        _ => path.to_string(),
    }
}

/// Load `~/.ssh/config`, returning an empty list on any error.
pub fn load_user_ssh_config() -> Vec<SshConfigHost> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    load_ssh_config_file(&home.join(".ssh").join("config"), Some(&home))
}

/// Load an OpenSSH config file with its `Include` directives inlined, the
/// way `ssh` reads them: `~` expands to `home`, relative paths resolve
/// against the config file's directory (`~/.ssh` for the user config), and
/// a `*`/`?` pattern in the file name pulls in every match in sorted order.
/// Missing files are skipped, as `ssh` does.
pub fn load_ssh_config_file(path: &Path, home: Option<&Path>) -> Vec<SshConfigHost> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let base = path.parent().unwrap_or(Path::new("."));
    let mut out = String::new();
    inline_includes(&text, base, home, 0, &mut out);
    // A file included twice yields the same block twice; `ssh` uses the
    // first, so keep only that one.
    let mut seen = std::collections::HashSet::new();
    parse_ssh_config_in(&out, home)
        .into_iter()
        .filter(|host| seen.insert(host.alias.clone()))
        .collect()
}

fn inline_includes(text: &str, base: &Path, home: Option<&Path>, depth: usize, out: &mut String) {
    for line in text.lines() {
        let directive = line.split('#').next().unwrap_or("").trim();
        let include = directive
            .split_once([' ', '\t', '='])
            .filter(|(keyword, _)| keyword.eq_ignore_ascii_case("include"));
        let Some((_, value)) = include else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        if depth >= MAX_INCLUDE_DEPTH {
            continue;
        }
        for pattern in value.split_whitespace() {
            let pattern = pattern.trim_matches('"');
            let expanded = PathBuf::from(expand_tilde(pattern, home));
            let full = if expanded.is_absolute() {
                expanded
            } else {
                base.join(expanded)
            };
            for file in matching_files(&full) {
                if let Ok(included) = std::fs::read_to_string(&file) {
                    inline_includes(&included, base, home, depth + 1, out);
                }
            }
        }
    }
}

/// Files matching `path`, whose last component may hold `*`/`?` wildcards.
fn matching_files(path: &Path) -> Vec<PathBuf> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    if !name.contains(['*', '?']) {
        return if path.is_file() {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|candidate| wildcard_match(name, candidate))
        })
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

fn wildcard_match(pattern: &str, candidate: &str) -> bool {
    fn go(p: &[char], c: &[char]) -> bool {
        match p.split_first() {
            None => c.is_empty(),
            Some(('*', rest)) => (0..=c.len()).any(|i| go(rest, &c[i..])),
            Some(('?', rest)) => !c.is_empty() && go(rest, &c[1..]),
            Some((ch, rest)) => c.first() == Some(ch) && go(rest, &c[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let c: Vec<char> = candidate.chars().collect();
    // Hidden files only match a pattern that names the dot, as with globs.
    (!candidate.starts_with('.') || pattern.starts_with('.')) && go(&p, &c)
}

impl SshConfigHost {
    pub fn to_hop(&self) -> SshHop {
        let host = self.host_name.clone().unwrap_or_else(|| self.alias.clone());
        let auth = match &self.identity_file {
            Some(path) => SshAuth::KeyFile {
                path: path.clone(),
                passphrase: None,
            },
            None => SshAuth::Agent,
        };
        SshHop {
            host,
            port: self.port.unwrap_or(22),
            username: self.user.clone().unwrap_or_default(),
            auth,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_hostname_port_user_identityfile() {
        let text = r#"
Host mybastion
    HostName bastion.example.com
    Port 2222
    User deploy
    IdentityFile ~/.ssh/id_ed25519
"#;
        let hosts = parse_ssh_config(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "mybastion");
        assert_eq!(hosts[0].host_name.as_deref(), Some("bastion.example.com"));
        assert_eq!(hosts[0].port, Some(2222));
        assert_eq!(hosts[0].user.as_deref(), Some("deploy"));
        // Compare by components: the expansion uses the native separator.
        assert!(
            Path::new(hosts[0].identity_file.as_ref().unwrap())
                .ends_with(Path::new(".ssh").join("id_ed25519"))
        );
    }

    #[test]
    fn skips_wildcard_and_multi_token_hosts() {
        let text = r#"
Host *
    User nobody
Host bastion prod
    HostName x
Host real
    HostName y
"#;
        let hosts = parse_ssh_config(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "real");
    }

    #[test]
    fn first_value_wins_within_block() {
        let text = r#"
Host h
    HostName first.example.com
    HostName second.example.com
    Port 22
    Port 2222
"#;
        let hosts = parse_ssh_config(text);
        assert_eq!(hosts[0].host_name.as_deref(), Some("first.example.com"));
        assert_eq!(hosts[0].port, Some(22));
    }

    #[test]
    fn keyword_case_and_equals_form() {
        let text = "Host h\nHOSTNAME=host.test\nport=2200\nUSER=me\n";
        let hosts = parse_ssh_config(text);
        assert_eq!(hosts[0].host_name.as_deref(), Some("host.test"));
        assert_eq!(hosts[0].port, Some(2200));
        assert_eq!(hosts[0].user.as_deref(), Some("me"));
    }

    fn write(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn include_with_tilde_and_glob_pulls_in_hosts() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        write(
            &ssh.join("config"),
            "Include ~/.ssh/conf.d/*.conf\nHost main\n    HostName main.test\n",
        );
        write(
            &ssh.join("conf.d/b.conf"),
            "Host beta\n    HostName beta.test\n    IdentityFile ~/.ssh/beta_key\n",
        );
        write(
            &ssh.join("conf.d/a.conf"),
            "Host alpha\n    HostName alpha.test\n",
        );
        write(&ssh.join("conf.d/ignored.txt"), "Host nope\n");

        let hosts = load_ssh_config_file(&ssh.join("config"), Some(home.path()));
        let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, ["alpha", "beta", "main"]);
        assert_eq!(
            hosts[1].identity_file.as_deref(),
            Some(ssh.join("beta_key").display().to_string().as_str())
        );
    }

    #[test]
    fn include_relative_path_resolves_against_ssh_dir_and_nests() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        write(&ssh.join("config"), "Include work extra/one\n");
        write(
            &ssh.join("work"),
            "Host work\n    HostName work.test\nInclude nested\n",
        );
        write(
            &ssh.join("nested"),
            "Host nested\n    HostName nested.test\n",
        );
        write(&ssh.join("extra/one"), "Host one\n    HostName one.test\n");

        let hosts = load_ssh_config_file(&ssh.join("config"), Some(home.path()));
        let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, ["work", "nested", "one"]);
    }

    #[test]
    fn include_cycle_and_missing_files_do_not_hang_or_fail() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        write(
            &ssh.join("config"),
            "Include config\nInclude ~/.ssh/missing\nHost h\n    HostName h.test\n",
        );
        let hosts = load_ssh_config_file(&ssh.join("config"), Some(home.path()));
        assert_eq!(hosts.iter().filter(|h| h.alias == "h").count(), 1);
    }

    #[test]
    fn to_hop_defaults_port_and_agent_auth() {
        let host = SshConfigHost {
            alias: "alias".to_string(),
            host_name: None,
            port: None,
            user: Some("u".to_string()),
            identity_file: None,
        };
        let hop = host.to_hop();
        assert_eq!(hop.host, "alias");
        assert_eq!(hop.port, 22);
        assert_eq!(hop.username, "u");
        assert!(matches!(hop.auth, SshAuth::Agent));
    }
}
