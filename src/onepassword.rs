//! Optional lookup of connection details in 1Password, through the official
//! `op` CLI (`op item get <name> --format json`).
//!
//! Only form fields the user left empty are filled, so anything typed always
//! wins. An item that doesn't exist means "nothing to fill" (the fields stay
//! empty, exactly as without 1Password). Fetched values are used for the one
//! connection and are never written to the form, the config or any log.

use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::{backend::Protocol, form::FormValues};

/// How long to wait for `op` (it may be waiting for the user to unlock 1Password).
const TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// Restrict the search to one vault (config: `onepassword_vault`).
    pub vault: Option<String>,
}

#[derive(Debug)]
pub enum Lookup {
    Found(Item),
    /// No such item: not an error, there is just nothing to fill in.
    NotFound,
    /// `op` is missing, locked, signed out, etc.: connect anyway, but say why.
    Unavailable(String),
}

#[derive(Debug, Default)]
pub struct Item {
    fields: Vec<Field>,
    urls: Vec<String>,
}

struct Field {
    /// Lower-cased label with everything but letters and digits removed.
    label: String,
    purpose: String,
    value: String,
}

// Values are secrets: show labels only.
impl std::fmt::Debug for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Field")
            .field("label", &self.label)
            .field("value", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize, Default)]
struct RawItem {
    #[serde(default)]
    fields: Vec<RawField>,
    #[serde(default)]
    urls: Vec<RawUrl>,
}

#[derive(Deserialize)]
struct RawField {
    #[serde(default)]
    label: String,
    #[serde(default)]
    purpose: Option<String>,
    #[serde(default)]
    value: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawUrl {
    #[serde(default)]
    href: String,
}

fn normalise(label: &str) -> String {
    label
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

impl Item {
    /// Parse `op item get --format json` output. The error carries no output
    /// text, which may contain secrets.
    pub fn parse(json: &str) -> Result<Item, &'static str> {
        let raw: RawItem = serde_json::from_str(json).map_err(|_| "unexpected 1Password output")?;
        let fields = raw
            .fields
            .into_iter()
            .filter_map(|f| {
                let value = match f.value? {
                    serde_json::Value::String(s) => s,
                    serde_json::Value::Number(n) => n.to_string(),
                    _ => return None,
                };
                (!value.trim().is_empty()).then(|| Field {
                    label: normalise(&f.label),
                    purpose: f.purpose.unwrap_or_default().to_uppercase(),
                    value: value.trim().to_string(),
                })
            })
            .collect();
        let urls = raw.urls.into_iter().map(|u| u.href).collect();
        Ok(Item { fields, urls })
    }

    fn by_purpose(&self, purpose: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|f| f.purpose == purpose)
            .map(|f| f.value.as_str())
    }

    /// First field whose (normalised) label is one of `labels`, in label order.
    fn by_label(&self, labels: &[&str]) -> Option<&str> {
        labels.iter().find_map(|l| {
            self.fields
                .iter()
                .find(|f| f.label == *l)
                .map(|f| f.value.as_str())
        })
    }

    /// Host of the item's first URL (`ssh://host:22/` or `host`).
    fn host_from_urls(&self) -> Option<String> {
        self.urls.iter().find_map(|u| {
            let rest = u.split_once("://").map_or(u.as_str(), |(_, r)| r);
            let host = rest.split(['/', '?']).next()?;
            let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
            let host = host.split(':').next()?;
            (!host.is_empty()).then(|| host.to_string())
        })
    }

    /// Fill the empty fields of `v` from this item. Never overwrites a value.
    pub fn fill(&self, v: &mut FormValues) {
        let empty = |s: &str| s.trim().is_empty();
        let set = |slot: &mut String, value: Option<&str>| {
            if empty(slot)
                && let Some(x) = value
            {
                *slot = x.to_string();
            }
        };

        match v.protocol {
            Protocol::S3 => {
                set(
                    &mut v.host,
                    self.by_label(&["endpoint", "endpointurl", "host", "url"]),
                );
                set(&mut v.region, self.by_label(&["region", "awsregion"]));
                set(&mut v.bucket, self.by_label(&["bucket", "bucketname"]));
                set(
                    &mut v.aws_profile,
                    self.by_label(&["awsprofile", "profile"]),
                );
                // Keys travel as a pair: never mix a typed key id with a fetched secret.
                if empty(&v.user) && empty(&v.pass) {
                    let key = self
                        .by_label(&["accesskeyid", "awsaccesskeyid", "accesskey", "username"])
                        .or_else(|| self.by_purpose("USERNAME"));
                    let secret = self
                        .by_label(&[
                            "secretaccesskey",
                            "awssecretaccesskey",
                            "secretkey",
                            "password",
                        ])
                        .or_else(|| self.by_purpose("PASSWORD"));
                    if let (Some(k), Some(s)) = (key, secret) {
                        v.user = k.to_string();
                        v.pass = s.to_string();
                    }
                }
            }
            Protocol::Sftp | Protocol::Ftp | Protocol::Ftps => {
                let host = self
                    .by_label(&["host", "hostname", "server", "address", "ip", "ipaddress"])
                    .map(str::to_string)
                    .or_else(|| self.host_from_urls());
                set(&mut v.host, host.as_deref());
                set(
                    &mut v.user,
                    self.by_purpose("USERNAME")
                        .or_else(|| self.by_label(&["username", "user", "login"])),
                );
                set(
                    &mut v.pass,
                    self.by_purpose("PASSWORD")
                        .or_else(|| self.by_label(&["password", "passphrase"])),
                );
                // The port field is pre-filled with the protocol default, so "empty"
                // means "still the default".
                if (empty(&v.port) || v.port.trim() == v.protocol.default_port().to_string())
                    && let Some(p) = self
                        .by_label(&["port"])
                        .filter(|p| p.trim().parse::<u16>().is_ok_and(|n| n != 0))
                {
                    v.port = p.trim().to_string();
                }
                if v.protocol == Protocol::Sftp {
                    set(
                        &mut v.key,
                        self.by_label(&["keyfile", "keypath", "privatekeypath", "identityfile"]),
                    );
                }
            }
        }
    }
}

/// Look up the item called `name`. `name` comes from the user, so refuse
/// anything `op` could mistake for an option.
pub async fn lookup(name: &str, settings: &Settings) -> Lookup {
    let name = name.trim();
    if name.is_empty() || name.starts_with('-') {
        return Lookup::NotFound;
    }
    let mut cmd = Command::new("op");
    cmd.args(["item", "get", name, "--format", "json", "--reveal"]);
    if let Some(vault) = settings.vault.as_deref().filter(|v| !v.is_empty()) {
        cmd.args(["--vault", vault]);
    }
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);

    let output = match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Err(_) => {
            return Lookup::Unavailable(
                "timed out waiting for 1Password (is it locked?)".to_string(),
            );
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Lookup::Unavailable("the 1Password CLI ('op') is not installed".to_string());
        }
        Ok(Err(e)) => return Lookup::Unavailable(format!("cannot run 'op': {e}")),
        Ok(Ok(o)) => o,
    };

    if output.status.success() {
        return match Item::parse(&String::from_utf8_lossy(&output.stdout)) {
            Ok(item) => Lookup::Found(item),
            Err(e) => Lookup::Unavailable(e.to_string()),
        };
    }
    classify_failure(&String::from_utf8_lossy(&output.stderr))
}

/// Tell "no such item" (fine) apart from "can't talk to 1Password" (warn).
fn classify_failure(stderr: &str) -> Lookup {
    let lower = stderr.to_lowercase();
    if lower.contains("isn't an item")
        || lower.contains("is not an item")
        || lower.contains("no item")
        || lower.contains("not found")
    {
        return Lookup::NotFound;
    }
    // `op` prefixes errors with a timestamp and `[ERROR]`; keep the message only.
    let line = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("unknown error");
    let msg = line.split("[ERROR]").last().unwrap_or(line);
    let msg: String = msg
        .trim()
        .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '/' | ':' | ' '))
        .chars()
        .filter(|c| !c.is_control())
        .take(140)
        .collect();
    Lookup::Unavailable(format!("1Password: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "id": "abc", "title": "prod-sftp",
      "urls": [{"href": "sftp://admin@files.example.com:2222/data", "primary": true}],
      "fields": [
        {"id":"username","type":"STRING","purpose":"USERNAME","label":"username","value":"deploy"},
        {"id":"password","type":"CONCEALED","purpose":"PASSWORD","label":"password","value":"s3cret!"},
        {"id":"p","type":"STRING","label":"Port","value":"2222"},
        {"id":"k","type":"STRING","label":"Key File","value":"~/.ssh/id_prod"},
        {"id":"n","type":"STRING","label":"notes","value":"  "},
        {"id":"o","type":"OTP","label":"one-time password","value":null}
      ]
    }"#;

    fn sftp() -> FormValues {
        FormValues {
            protocol: Protocol::Sftp,
            port: "22".into(),
            ..FormValues::default()
        }
    }

    #[test]
    fn fills_only_empty_fields() {
        let item = Item::parse(SAMPLE).unwrap();
        let mut v = sftp();
        v.user = "typed-user".into();
        item.fill(&mut v);
        assert_eq!(v.user, "typed-user", "typed values always win");
        assert_eq!(v.pass, "s3cret!");
        assert_eq!(
            v.host, "files.example.com",
            "host comes from the item's URL"
        );
        assert_eq!(
            v.port, "2222",
            "a default-valued port counts as unspecified"
        );
        assert_eq!(v.key, "~/.ssh/id_prod");
    }

    #[test]
    fn typed_port_and_host_are_kept() {
        let item = Item::parse(SAMPLE).unwrap();
        let mut v = sftp();
        v.host = "mine.example.com".into();
        v.port = "2200".into();
        item.fill(&mut v);
        assert_eq!(v.host, "mine.example.com");
        assert_eq!(v.port, "2200");
    }

    #[test]
    fn labelled_host_beats_url() {
        let item = Item::parse(
            r#"{"urls":[{"href":"https://wrong.example"}],
                "fields":[{"label":"Hostname","value":"right.example"}]}"#,
        )
        .unwrap();
        let mut v = sftp();
        item.fill(&mut v);
        assert_eq!(v.host, "right.example");
    }

    #[test]
    fn missing_values_stay_empty() {
        let item = Item::parse(r#"{"fields":[{"label":"note","value":"hi"}]}"#).unwrap();
        let mut v = sftp();
        item.fill(&mut v);
        assert!(v.user.is_empty() && v.pass.is_empty() && v.host.is_empty());
        assert_eq!(v.port, "22");
        // Blank and null values are ignored.
        let item = Item::parse(SAMPLE).unwrap();
        assert!(item.by_label(&["notes", "onetimepassword"]).is_none());
    }

    #[test]
    fn s3_fields_and_key_pairing() {
        let item = Item::parse(
            r#"{"fields":[
                {"label":"Access Key ID","value":"AKIA1"},
                {"label":"Secret Access Key","value":"SEKRET"},
                {"label":"region","value":"eu-west-2"},
                {"label":"Bucket","value":"assets"},
                {"label":"Endpoint URL","value":"https://r2.example"}]}"#,
        )
        .unwrap();
        let mut v = FormValues {
            protocol: Protocol::S3,
            ..FormValues::default()
        };
        item.fill(&mut v);
        assert_eq!((v.user.as_str(), v.pass.as_str()), ("AKIA1", "SEKRET"));
        assert_eq!(v.region, "eu-west-2");
        assert_eq!(v.bucket, "assets");
        assert_eq!(v.host, "https://r2.example");

        // A typed key id is never paired with a secret from 1Password.
        let mut typed = FormValues {
            protocol: Protocol::S3,
            user: "MYKEY".into(),
            ..FormValues::default()
        };
        item.fill(&mut typed);
        assert_eq!(typed.user, "MYKEY");
        assert!(typed.pass.is_empty());
    }

    #[test]
    fn url_host_extraction() {
        let host = |u: &str| {
            Item {
                fields: vec![],
                urls: vec![u.to_string()],
            }
            .host_from_urls()
        };
        assert_eq!(
            host("sftp://u@h.example:22/x").as_deref(),
            Some("h.example")
        );
        assert_eq!(host("h.example").as_deref(), Some("h.example"));
        assert_eq!(
            host("https://h.example/path?q=1").as_deref(),
            Some("h.example")
        );
        assert_eq!(host("").as_deref(), None);
    }

    #[test]
    fn failure_classification() {
        assert!(matches!(
            classify_failure("[ERROR] 2026/10/26 12:00:00 \"nope\" isn't an item in any vault."),
            Lookup::NotFound
        ));
        assert!(matches!(
            classify_failure("[ERROR] 2026/10/26 12:00:00 No items found"),
            Lookup::NotFound
        ));
        match classify_failure(
            "[ERROR] 2026/10/26 12:00:00 You are not signed in. Run `op signin`.",
        ) {
            Lookup::Unavailable(m) => assert!(m.contains("not signed in"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_errors_do_not_leak_output() {
        let e = Item::parse("password=hunter2 not json").unwrap_err();
        assert!(!e.contains("hunter2"));
    }

    #[tokio::test]
    async fn option_like_names_are_refused_without_running_op() {
        assert!(matches!(
            lookup("--vault=evil", &Settings::default()).await,
            Lookup::NotFound
        ));
        assert!(matches!(
            lookup("  ", &Settings::default()).await,
            Lookup::NotFound
        ));
    }
}
