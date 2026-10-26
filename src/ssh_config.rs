//! Minimal OpenSSH client configuration (`~/.ssh/config`) resolution.
//!
//! Supported: `Host` (wildcards, `!negation`), `HostName`, `User`, `Port`,
//! `IdentityFile`, `IdentitiesOnly`, `Include` (with `*`/`?` globs), and the
//! `%h %u %r %d %%` / `~` expansions. As in OpenSSH, the first value obtained
//! for a setting wins and `IdentityFile` accumulates. `Match` blocks are not
//! evaluated (their contents are ignored). `ProxyJump`/`ProxyCommand` are
//! reported so the caller can refuse instead of connecting to the wrong place.

use std::{
    fs,
    path::{Path, PathBuf},
};

use directories::BaseDirs;

const MAX_INCLUDE_DEPTH: usize = 8;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<PathBuf>,
    pub identities_only: bool,
    /// `IdentityAgent`: a socket path, or `none` to disable the agent. `None` means
    /// the default (`SSH_AUTH_SOCK`).
    pub identity_agent: Option<String>,
    /// `ProxyJump` / `ProxyCommand` value, if one applies (and isn't `none`).
    pub proxy: Option<String>,
}

/// Resolve `alias` against the user's `~/.ssh/config`. A missing file yields
/// an empty result. Always empty under `cfg(test)` so unit tests don't depend
/// on the machine they run on.
pub fn resolve_for_user(alias: &str) -> Resolved {
    if cfg!(test) {
        return Resolved::default();
    }
    let Some(home) = BaseDirs::new().map(|d| d.home_dir().to_path_buf()) else {
        return Resolved::default();
    };
    resolve(alias, &home.join(".ssh").join("config"), &home)
}

pub fn resolve(alias: &str, config: &Path, home: &Path) -> Resolved {
    let mut st = State {
        alias: alias.to_string(),
        home: home.to_path_buf(),
        active: true,
        raw_identities: Vec::new(),
        identities_only: None,
        identity_agent: None,
        proxy: None,
        out: Resolved::default(),
    };
    load(config, 0, &mut st);

    let host = st.out.hostname.clone().unwrap_or_else(|| alias.to_string());
    let user = st.out.user.clone().unwrap_or_default();
    st.out.identity_files = st
        .raw_identities
        .iter()
        .map(|s| expand(s, &host, &user, home))
        .map(PathBuf::from)
        .collect();
    if let Some(h) = st.out.hostname.take() {
        st.out.hostname = Some(expand(&h, alias, &user, home));
    }
    st.out.identities_only = st.identities_only.unwrap_or(false);
    st.out.identity_agent = st
        .identity_agent
        .as_deref()
        .map(|a| expand_agent(a, alias, &user, home));
    st.out.proxy = st.proxy.filter(|p| !p.eq_ignore_ascii_case("none"));
    st.out
}

struct State {
    alias: String,
    home: PathBuf,
    /// Whether directives in the current block apply to `alias`.
    active: bool,
    raw_identities: Vec<String>,
    identities_only: Option<bool>,
    identity_agent: Option<String>,
    proxy: Option<String>,
    out: Resolved,
}

fn load(path: &Path, depth: usize, st: &mut State) {
    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((keyword, args)) = split_keyword(line) else {
            continue;
        };
        match keyword.as_str() {
            "host" => st.active = host_matches(&args, &st.alias),
            // Not evaluated: ignore everything until the next Host line.
            "match" => st.active = false,
            "include" => {
                if st.active {
                    for pattern in &args {
                        for file in expand_include(pattern, &st.home) {
                            load(&file, depth + 1, st);
                        }
                    }
                }
            }
            _ if !st.active => {}
            "hostname" => set_once(&mut st.out.hostname, args.first().cloned()),
            "user" => set_once(&mut st.out.user, args.first().cloned()),
            "port" => {
                if st.out.port.is_none() {
                    st.out.port = args
                        .first()
                        .and_then(|p| p.parse().ok())
                        .filter(|p| *p != 0);
                }
            }
            "identityfile" => {
                if let Some(f) = args.first() {
                    st.raw_identities.push(f.clone());
                }
            }
            "identitiesonly" => {
                if st.identities_only.is_none() {
                    st.identities_only = args.first().map(|v| v.eq_ignore_ascii_case("yes"));
                }
            }
            "identityagent" => set_once(&mut st.identity_agent, args.first().cloned()),
            "proxyjump" | "proxycommand" => {
                set_once(&mut st.proxy, Some(args.join(" ")));
            }
            _ => {}
        }
    }
}

fn set_once(slot: &mut Option<String>, value: Option<String>) {
    if slot.is_none() {
        *slot = value;
    }
}

/// `Keyword arg arg` or `Keyword=arg`; arguments may be double-quoted.
fn split_keyword(line: &str) -> Option<(String, Vec<String>)> {
    let end = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let keyword = line[..end].to_ascii_lowercase();
    let rest = line[end..].trim_start_matches(|c: char| c.is_whitespace() || c == '=');
    Some((keyword, split_args(rest)))
}

fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut has = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                has = true;
            }
            '\\' if !quoted => match chars.peek() {
                // OpenSSH accepts `\ ` (and escaped quotes/backslashes) in arguments.
                Some(&n) if matches!(n, ' ' | '"' | '\'' | '\\') => {
                    cur.push(n);
                    chars.next();
                    has = true;
                }
                _ => {
                    cur.push('\\');
                    has = true;
                }
            },
            '#' if !quoted && !has => break, // trailing comment
            c if c.is_whitespace() && !quoted => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

fn host_matches(patterns: &[String], host: &str) -> bool {
    let mut matched = false;
    for p in patterns {
        if let Some(neg) = p.strip_prefix('!') {
            if wildcard_match(neg, host) {
                return false;
            }
        } else if wildcard_match(p, host) {
            matched = true;
        }
    }
    matched
}

/// Case-insensitive match supporting `*` and `?`.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|i| go(rest, &t[i..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    go(&p, &t)
}

fn expand(s: &str, host: &str, user: &str, home: &Path) -> String {
    let mut out = String::new();
    let rest = if let Some(r) = s.strip_prefix("~/") {
        out.push_str(&home.to_string_lossy());
        out.push('/');
        r
    } else {
        s
    };
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('h') => out.push_str(host),
            Some('u' | 'r') => out.push_str(user),
            Some('d') => out.push_str(&home.to_string_lossy()),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// `IdentityAgent` value: `none`, `SSH_AUTH_SOCK`, or a path with `~`, `%`
/// tokens and `${ENV}` references.
fn expand_agent(value: &str, host: &str, user: &str, home: &Path) -> String {
    if value.eq_ignore_ascii_case("none") {
        return "none".into();
    }
    if value == "SSH_AUTH_SOCK" {
        return std::env::var("SSH_AUTH_SOCK").unwrap_or_else(|_| "none".into());
    }
    let mut out = expand(value, host, user, home);
    while let Some(start) = out.find("${") {
        let Some(len) = out[start..].find('}') else {
            break;
        };
        let name = &out[start + 2..start + len];
        let val = std::env::var(name).unwrap_or_default();
        out.replace_range(start..=start + len, &val);
    }
    out
}

/// Files matched by an `Include` argument (relative paths are under `~/.ssh`).
fn expand_include(pattern: &str, home: &Path) -> Vec<PathBuf> {
    let full = if let Some(r) = pattern.strip_prefix("~/") {
        home.join(r)
    } else if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        home.join(".ssh").join(pattern)
    };
    let Some(name) = full.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    if !name.contains(['*', '?']) {
        return vec![full];
    }
    let Some(dir) = full.parent() else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| wildcard_match(name, n))
        })
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "remotefiletui-sshcfg-{}-{name}",
            std::process::id()
        ));
        fs::create_dir_all(d.join(".ssh/conf.d")).unwrap();
        d
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.example.com", "a.example.com"));
        assert!(wildcard_match("web?", "web1"));
        assert!(!wildcard_match("web?", "web12"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("HOST", "host"));
        assert!(!wildcard_match("a*b", "acd"));
    }

    #[test]
    fn host_patterns_with_negation() {
        let pats = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(host_matches(&pats(&["*.corp", "!secret.corp"]), "web.corp"));
        assert!(!host_matches(
            &pats(&["*.corp", "!secret.corp"]),
            "secret.corp"
        ));
        assert!(!host_matches(&pats(&["!x"]), "y"));
    }

    #[test]
    fn first_value_wins_and_identity_accumulates() {
        let home = tmp("first");
        fs::write(
            home.join(".ssh/config"),
            "Host box\n  HostName 10.0.0.5\n  User alice\n  Port 2200\n  IdentityFile ~/.ssh/a\n\n\
             Host *\n  User bob\n  Port 22\n  IdentityFile ~/.ssh/b\n  IdentitiesOnly yes\n",
        )
        .unwrap();
        let r = resolve("box", &home.join(".ssh/config"), &home);
        assert_eq!(r.hostname.as_deref(), Some("10.0.0.5"));
        assert_eq!(r.user.as_deref(), Some("alice"));
        assert_eq!(r.port, Some(2200));
        assert_eq!(
            r.identity_files,
            vec![home.join(".ssh/a"), home.join(".ssh/b")]
        );
        assert!(r.identities_only);
        // A host with no specific block only gets the wildcard block.
        let other = resolve("other", &home.join(".ssh/config"), &home);
        assert_eq!(other.user.as_deref(), Some("bob"));
        assert_eq!(other.hostname, None);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn include_quotes_equals_and_comments() {
        let home = tmp("inc");
        fs::write(
            home.join(".ssh/config"),
            "Include conf.d/*.conf\nHost=late\n  Port=2022 # trailing\n",
        )
        .unwrap();
        fs::write(
            home.join(".ssh/conf.d/a.conf"),
            "Host inc\n  HostName \"my host.example\"\n  IdentityFile %d/.ssh/key_%h\n",
        )
        .unwrap();
        fs::write(
            home.join(".ssh/conf.d/ignored.txt"),
            "Host inc\n  User nope\n",
        )
        .unwrap();
        let r = resolve("inc", &home.join(".ssh/config"), &home);
        assert_eq!(r.hostname.as_deref(), Some("my host.example"));
        assert_eq!(r.user, None);
        assert_eq!(
            r.identity_files,
            vec![home.join(".ssh/key_my host.example")]
        );
        let late = resolve("late", &home.join(".ssh/config"), &home);
        assert_eq!(late.port, Some(2022));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn proxy_is_reported_and_match_ignored() {
        let home = tmp("proxy");
        fs::write(
            home.join(".ssh/config"),
            "Host inner\n  ProxyJump bastion\n\nMatch host inner\n  User ignored\nHost off\n  ProxyJump none\n",
        )
        .unwrap();
        let r = resolve("inner", &home.join(".ssh/config"), &home);
        assert_eq!(r.proxy.as_deref(), Some("bastion"));
        assert_eq!(r.user, None);
        assert_eq!(resolve("off", &home.join(".ssh/config"), &home).proxy, None);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn missing_file_and_include_loop_are_safe() {
        let home = tmp("loop");
        assert_eq!(
            resolve("x", &home.join(".ssh/nope"), &home),
            Resolved::default()
        );
        fs::write(
            home.join(".ssh/config"),
            "Include config\nHost x\n  User u\n",
        )
        .unwrap();
        assert_eq!(
            resolve("x", &home.join(".ssh/config"), &home)
                .user
                .as_deref(),
            Some("u")
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn identity_agent_with_escaped_space() {
        let home = tmp("agent");
        fs::write(
            home.join(".ssh/config"),
            "Host *\n\tIdentityAgent ~/Library/Group\\ Containers/x/agent.sock\nHost off\n  IdentityAgent none\n",
        )
        .unwrap();
        let r = resolve("anything", &home.join(".ssh/config"), &home);
        assert_eq!(
            r.identity_agent,
            Some(format!(
                "{}/Library/Group Containers/x/agent.sock",
                home.display()
            ))
        );
        // `Host *` came first, so first-value-wins applies to later blocks too.
        let off = resolve("off", &home.join(".ssh/config"), &home);
        assert!(off.identity_agent.unwrap().ends_with("agent.sock"));
        assert_eq!(split_args(r"a\ b c"), vec!["a b", "c"]);
        fs::remove_dir_all(home).unwrap();
    }
}
