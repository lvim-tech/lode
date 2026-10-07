// lode/access: where AWS credentials come from.
//
// The AWS drivers (DynamoDB, CloudWatch, S3, Redshift) all need the same thing,
// and nobody wants to paste an access key into a connection form when the
// machine already has `~/.aws/config` with eight profiles in it. So the engine
// reads those files and offers what it finds.
//
// WHAT IS HONEST HERE AND WHAT IS NOT, said once, at the top:
//
//   * Parsing `~/.aws/config` and `~/.aws/credentials` is pure file reading and
//     is tested properly.
//   * `credential_process` is a program the AWS tools already run for exactly
//     this, it is spawned and its JSON read, and it is tested properly.
//   * Static keys, session tokens and `source_profile` chains are resolved here
//     and tested properly.
//   * SSO and AssumeRole need to TALK to AWS. They are recognised, reported,
//     and REFUSED with a message saying what would have to happen — because a
//     silent fall-through to "no credentials" would look like a missing profile
//     rather than an unsupported one.
//
// Nothing here is verified against the real AWS. The parsing is verified against
// files; the network half does not exist rather than existing untested.
//
// A credential this produces is a VALUE, not a template, and it is produced at
// connect time and dropped after. Nothing here is ever written to the store:
// what the store keeps is the PROFILE NAME, which is a reference, and resolving
// it again tomorrow is what makes a rotated key work without editing anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// One profile as the AWS files describe it.
///
/// Flat and string-valued on purpose: these files gain keys faster than any
/// reader can model them, and a profile this does not fully understand should
/// still be listed and still be nameable rather than dropped for having a key
/// nobody has heard of.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Profile {
    pub name: String,
    /// Which file it came from, for a client that wants to say so.
    pub source: String,
    #[serde(flatten)]
    pub keys: BTreeMap<String, String>,
}

impl Profile {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.keys.get(k).map(String::as_str)
    }

    /// How this profile would be resolved, as one word — so a client can show a
    /// list and say which entries will actually work.
    pub fn kind(&self) -> &'static str {
        if self.get("aws_access_key_id").is_some() {
            "static"
        } else if self.get("credential_process").is_some() {
            "process"
        } else if self.get("sso_start_url").is_some() || self.get("sso_session").is_some() {
            "sso"
        } else if self.get("role_arn").is_some() {
            "assume_role"
        } else if self.get("source_profile").is_some() {
            "inherited"
        } else {
            "none"
        }
    }

    /// Whether this engine can actually produce credentials from it.
    pub fn usable(&self) -> bool {
        matches!(self.kind(), "static" | "process" | "inherited")
    }
}

/// Resolved credentials. Never stored, never logged, never put in an
/// environment: produced at connect time and dropped after.
#[derive(Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// From the profile, if it names one. A client may still override it.
    pub region: Option<String>,
}

impl std::fmt::Debug for Credentials {
    /// Redacted, for the same reason `Secret` is: so that a whole struct can go
    /// into an error or a log line without anybody having to remember.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &"redacted")
            .field("secret_access_key", &"redacted")
            .field("session_token", &self.session_token.as_ref().map(|_| "redacted"))
            .field("region", &self.region)
            .finish()
    }
}

/// Where the AWS files are, in probe order. `$AWS_CONFIG_FILE` and
/// `$AWS_SHARED_CREDENTIALS_FILE` are what the AWS tools themselves honour, so
/// honouring them means one answer rather than two.
pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AWS_CONFIG_FILE") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".aws/config"))
}

pub fn credentials_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AWS_SHARED_CREDENTIALS_FILE") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".aws/credentials"))
}

/// Parse one AWS INI file.
///
/// Written out rather than taken from an ini crate because this format is not
/// ini: `[profile dev]` in one file is `[dev]` in the other, values may be
/// indented continuations of the key above, and `#` and `;` both start a
/// comment. A general reader would need all of that configured anyway, and
/// then the behaviour would be in its documentation rather than here.
pub fn parse(text: &str, source: &str, strip_profile_prefix: bool) -> Vec<Profile> {
    let mut out: Vec<Profile> = Vec::new();
    let mut cur: Option<Profile> = None;
    // Nested blocks, as a STACK keyed by indentation. `services` opens one and
    // `s3` opens another inside it, so a single "current block" turns
    // `services.s3.endpoint_url` into `s3.endpoint_url` and loses which service
    // it was about. The depth is the indentation, which is the only thing these
    // files use to say it.
    let mut stack: Vec<(usize, String)> = Vec::new();

    for raw in text.lines() {
        let line = raw.split(['#', ';']).next().unwrap_or("");
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if let Some(p) = cur.take() {
                out.push(p);
            }
            stack.clear();
            let inner = trimmed[1..trimmed.len() - 1].trim();
            // `[profile dev]` in config, `[dev]` in credentials. `[default]` is
            // itself in both.
            let name = if strip_profile_prefix {
                inner.strip_prefix("profile ").map(str::trim).unwrap_or(inner)
            } else {
                inner
            };
            // Sections that are not profiles at all — `[sso-session x]`,
            // `[services x]`. Kept as their own entries so a client can show
            // them, but they will not resolve, which `kind()` already says.
            cur = Some(Profile {
                name: name.to_string(),
                source: source.to_string(),
                keys: BTreeMap::new(),
            });
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let indent = line.len() - line.trim_start().len();
        // Anything at this indentation or shallower closes the blocks it is no
        // longer inside.
        while stack.last().map(|(d, _)| indent <= *d).unwrap_or(false) {
            stack.pop();
        }
        // A key with an EMPTY value opens a block rather than setting anything.
        // It is not stored: `services = ""` is not a setting, it is a heading.
        if v.is_empty() {
            stack.push((indent, k.to_string()));
            continue;
        }
        let key = if stack.is_empty() {
            k.to_string()
        } else {
            let mut path: Vec<&str> = stack.iter().map(|(_, n)| n.as_str()).collect();
            path.push(k);
            path.join(".")
        };
        if let Some(p) = cur.as_mut() {
            p.keys.insert(key, v.to_string());
        }
    }
    if let Some(p) = cur {
        out.push(p);
    }
    out
}

/// Every profile the two files describe, merged by name.
///
/// `~/.aws/credentials` wins a clash key by key, which is what the AWS tools do
/// and, more to the point, is what somebody means when they put a key in the
/// credentials file and everything else in the config file.
pub fn profiles() -> Vec<Profile> {
    let mut merged: BTreeMap<String, Profile> = BTreeMap::new();
    for (path, strip) in [(config_path(), true), (credentials_path(), false)] {
        let Some(path) = path else { continue };
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for p in parse(&text, &path.display().to_string(), strip) {
            let e = merged.entry(p.name.clone()).or_insert_with(|| Profile {
                name: p.name.clone(),
                source: p.source.clone(),
                keys: BTreeMap::new(),
            });
            e.source = p.source.clone();
            e.keys.extend(p.keys);
        }
    }
    merged.into_values().collect()
}

/// Resolve one profile to credentials.
///
/// `depth` guards `source_profile` chains: they can point at each other, and a
/// loop here would be an engine that hangs on a configuration file.
pub async fn resolve(all: &[Profile], name: &str, depth: usize) -> anyhow::Result<Credentials> {
    if depth > 4 {
        anyhow::bail!("the AWS profile '{name}' chains through source_profile more than four deep — that is a loop");
    }
    let p = all
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| anyhow::anyhow!("there is no AWS profile called '{name}'"))?;
    let region = p.get("region").map(str::to_string);

    if let (Some(id), Some(secret)) = (p.get("aws_access_key_id"), p.get("aws_secret_access_key")) {
        return Ok(Credentials {
            access_key_id: id.to_string(),
            secret_access_key: secret.to_string(),
            session_token: p.get("aws_session_token").map(str::to_string),
            region,
        });
    }
    if let Some(cmd) = p.get("credential_process") {
        let mut c = credential_process(cmd).await?;
        c.region = c.region.or(region);
        return Ok(c);
    }
    if let Some(parent) = p.get("source_profile") {
        // No role is assumed — that needs STS, which needs the network. The
        // parent's own credentials come back, and the REFUSAL below is what
        // says the role was not taken, rather than letting somebody believe
        // they are acting as it.
        if p.get("role_arn").is_some() {
            anyhow::bail!(
                "the AWS profile '{name}' assumes the role {} through '{parent}', and this engine \
                 does not call STS — use `credential_process`, or a profile with keys of its own",
                p.get("role_arn").unwrap_or("")
            );
        }
        let mut c = Box::pin(resolve(all, parent, depth + 1)).await?;
        c.region = c.region.or(region);
        return Ok(c);
    }
    if p.get("sso_start_url").is_some() || p.get("sso_session").is_some() {
        anyhow::bail!(
            "the AWS profile '{name}' signs in through SSO, and this engine does not — run \
             `aws sso login --profile {name}` and give the profile a `credential_process`, or use \
             a profile with keys of its own"
        );
    }
    anyhow::bail!("the AWS profile '{name}' has no credentials this engine can use (it is '{}')", p.kind())
}

/// Run a `credential_process` and read what it prints.
///
/// The documented contract: it writes one JSON object on stdout with
/// `Version`, `AccessKeyId`, `SecretAccessKey` and optionally `SessionToken`.
/// Its STDERR is deliberately NOT captured into the error — a credential helper
/// prints prompts and progress there, and a password prompt quoted back inside
/// an error message is a password in a log.
async fn credential_process(cmd: &str) -> anyhow::Result<Credentials> {
    let out = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("the credential_process could not be started: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "the credential_process exited with {} — it printed nothing usable",
            out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "a signal".into())
        );
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow::anyhow!("the credential_process did not print JSON: {e}"))?;
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let (Some(id), Some(secret)) = (get("AccessKeyId"), get("SecretAccessKey")) else {
        anyhow::bail!("the credential_process printed JSON with no AccessKeyId and SecretAccessKey in it");
    };
    Ok(Credentials {
        access_key_id: id,
        secret_access_key: secret,
        session_token: get("SessionToken"),
        region: None,
    })
}

/// Read the two files from explicit paths — what the tests use, and what a
/// client importing from somewhere unusual would use.
pub fn profiles_from(config: Option<&Path>, credentials: Option<&Path>) -> Vec<Profile> {
    let mut merged: BTreeMap<String, Profile> = BTreeMap::new();
    for (path, strip) in [(config, true), (credentials, false)] {
        let Some(path) = path else { continue };
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        for p in parse(&text, &path.display().to_string(), strip) {
            let e = merged.entry(p.name.clone()).or_insert_with(|| Profile {
                name: p.name.clone(),
                source: p.source.clone(),
                keys: BTreeMap::new(),
            });
            e.source = p.source.clone();
            e.keys.extend(p.keys);
        }
    }
    merged.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
# the default one
[default]
region = eu-central-1
output = json

[profile dev]
region = eu-west-1
credential_process = sh -c 'echo {}'

[profile prod]
region = us-east-1
role_arn = arn:aws:iam::123456789012:role/Admin
source_profile = dev

[profile ssoish]
sso_session = corp
sso_account_id = 123456789012
sso_role_name = ReadOnly

[sso-session corp]
sso_start_url = https://corp.awsapps.com/start
sso_region = eu-central-1

[profile inherits]
source_profile = default
"#;

    const CREDS: &str = r#"
[default]
aws_access_key_id = AKIADEFAULT
aws_secret_access_key = secret-default

[keyed]
aws_access_key_id = AKIAKEYED
aws_secret_access_key = secret-keyed
aws_session_token = tok-keyed
"#;

    fn all() -> Vec<Profile> {
        let mut merged: BTreeMap<String, Profile> = BTreeMap::new();
        for (text, src, strip) in [(CONFIG, "config", true), (CREDS, "credentials", false)] {
            for p in parse(text, src, strip) {
                let e = merged.entry(p.name.clone()).or_insert_with(|| Profile {
                    name: p.name.clone(),
                    source: src.into(),
                    keys: BTreeMap::new(),
                });
                e.source = src.into();
                e.keys.extend(p.keys);
            }
        }
        merged.into_values().collect()
    }

    /// `[profile dev]` in config is `[dev]` in credentials, and `[default]` is
    /// itself in both. Getting that wrong gives a profile list with every name
    /// doubled.
    #[test]
    fn the_two_files_name_their_sections_differently_and_merge_to_one_list() {
        let all = all();
        let names: Vec<&str> = all.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"dev"), "{names:?}");
        assert!(names.contains(&"prod"), "{names:?}");
        assert!(names.contains(&"keyed"), "{names:?}");
        assert_eq!(names.iter().filter(|n| **n == "default").count(), 1, "{names:?}");
    }

    /// The credentials file wins key by key — which is what somebody means when
    /// they put the keys in one file and the region in the other.
    #[test]
    fn the_credentials_file_supplies_the_keys_and_the_config_the_region() {
        let all = all();
        let d = all.iter().find(|p| p.name == "default").unwrap();
        assert_eq!(d.get("aws_access_key_id"), Some("AKIADEFAULT"));
        assert_eq!(d.get("region"), Some("eu-central-1"));
    }

    #[test]
    fn comments_and_blank_lines_are_not_settings() {
        let p = parse("# [profile ghost]\n[real]\nk = v ; trailing\n", "x", false);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].name, "real");
        assert_eq!(p[0].get("k"), Some("v"));
    }

    /// An indented block under a key keeps its keys under a prefix instead of
    /// flattening them onto the profile, where `sso_region` from a session
    /// would silently become the profile's own region.
    #[test]
    fn a_nested_block_does_not_leak_into_the_profile() {
        let p = parse(
            "[profile x]\nregion = eu-west-1\nservices =\n  s3 =\n    endpoint_url = http://localhost:4566\n",
            "x",
            true,
        );
        assert_eq!(p[0].get("region"), Some("eu-west-1"));
        // The WHOLE path, so which service it was about survives — one level of
        // nesting would have made this `s3.endpoint_url` and lost `services`.
        assert_eq!(p[0].get("services.s3.endpoint_url"), Some("http://localhost:4566"));
        // And a heading is not a setting.
        assert!(p[0].get("services").is_none(), "{:?}", p[0].keys);
        assert!(p[0].get("endpoint_url").is_none(), "it must not land on the profile itself");
    }

    /// What a client shows beside each name. A profile that cannot work should
    /// say so in the list rather than when somebody picks it.
    #[test]
    fn every_profile_says_what_kind_it_is_and_whether_it_can_work() {
        let all = all();
        let kind = |n: &str| all.iter().find(|p| p.name == n).unwrap().kind();
        assert_eq!(kind("default"), "static");
        assert_eq!(kind("keyed"), "static");
        assert_eq!(kind("dev"), "process");
        assert_eq!(kind("prod"), "assume_role");
        assert_eq!(kind("ssoish"), "sso");
        assert_eq!(kind("inherits"), "inherited");
        let usable = |n: &str| all.iter().find(|p| p.name == n).unwrap().usable();
        assert!(usable("default") && usable("dev") && usable("inherits"));
        assert!(!usable("prod") && !usable("ssoish"));
    }

    #[tokio::test]
    async fn static_keys_resolve_with_their_session_token_and_region() {
        let c = resolve(&all(), "keyed", 0).await.unwrap();
        assert_eq!(c.access_key_id, "AKIAKEYED");
        assert_eq!(c.session_token.as_deref(), Some("tok-keyed"));
        let d = resolve(&all(), "default", 0).await.unwrap();
        assert_eq!(d.region.as_deref(), Some("eu-central-1"));
    }

    /// `source_profile` with no role is an inheritance, and the child's region
    /// does not overwrite the parent's credentials.
    #[tokio::test]
    async fn a_source_profile_without_a_role_inherits_the_parents_keys() {
        let c = resolve(&all(), "inherits", 0).await.unwrap();
        assert_eq!(c.access_key_id, "AKIADEFAULT");
    }

    /// A chain that points at itself must not hang the engine on a config file.
    #[tokio::test]
    async fn a_source_profile_loop_is_refused_rather_than_followed() {
        let looped = parse("[profile a]\nsource_profile = b\n[profile b]\nsource_profile = a\n", "x", true);
        let e = resolve(&looped, "a", 0).await.unwrap_err().to_string();
        assert!(e.contains("loop"), "{e}");
    }

    /// The two that need the network are REFUSED, by name, with what would have
    /// to happen instead. A silent "no credentials" would read as a missing
    /// profile rather than an unsupported one.
    #[tokio::test]
    async fn sso_and_assume_role_are_refused_with_what_to_do_about_it() {
        let e = resolve(&all(), "ssoish", 0).await.unwrap_err().to_string();
        assert!(e.contains("SSO"), "{e}");
        assert!(e.contains("aws sso login"), "{e}");
        let e = resolve(&all(), "prod", 0).await.unwrap_err().to_string();
        assert!(e.contains("STS"), "{e}");
        assert!(e.contains("role/Admin"), "it should name the role: {e}");
    }

    #[tokio::test]
    async fn a_profile_that_is_not_there_says_its_name() {
        let e = resolve(&all(), "nope", 0).await.unwrap_err().to_string();
        assert!(e.contains("'nope'"), "{e}");
    }

    /// The real contract of a credential helper, run for real.
    #[tokio::test]
    async fn a_credential_process_is_run_and_its_json_read() {
        let p = parse(
            "[profile helper]\ncredential_process = printf '{\"Version\":1,\"AccessKeyId\":\"AKIAPROC\",\"SecretAccessKey\":\"s3cr3t\",\"SessionToken\":\"tok\"}'\n",
            "x",
            true,
        );
        let c = resolve(&p, "helper", 0).await.unwrap();
        assert_eq!(c.access_key_id, "AKIAPROC");
        assert_eq!(c.session_token.as_deref(), Some("tok"));
    }

    #[tokio::test]
    async fn a_credential_process_that_fails_or_prints_rubbish_says_which() {
        let p = parse("[profile bad]\ncredential_process = exit 3\n", "x", true);
        let e = resolve(&p, "bad", 0).await.unwrap_err().to_string();
        assert!(e.contains("exited with 3"), "{e}");

        let p = parse("[profile junk]\ncredential_process = echo not-json\n", "x", true);
        let e = resolve(&p, "junk", 0).await.unwrap_err().to_string();
        assert!(e.contains("did not print JSON"), "{e}");

        let p = parse("[profile short]\ncredential_process = printf '{\"Version\":1}'\n", "x", true);
        let e = resolve(&p, "short", 0).await.unwrap_err().to_string();
        assert!(e.contains("no AccessKeyId"), "{e}");
    }

    /// Credentials must be safe to print by accident, like `Secret` is.
    #[test]
    fn credentials_do_not_print_themselves() {
        let c = Credentials {
            access_key_id: "AKIAREAL".into(),
            secret_access_key: "very-secret".into(),
            session_token: Some("A-SESSION-TOKEN".into()),
            region: Some("eu-west-1".into()),
        };
        let shown = format!("{c:?}");
        assert!(!shown.contains("AKIAREAL"), "{shown}");
        assert!(!shown.contains("very-secret"), "{shown}");
        // Not "tok": that is a substring of the FIELD NAME `session_token`, and
        // the first version of this assertion failed on its own label.
        assert!(!shown.contains("A-SESSION-TOKEN"), "{shown}");
        // The region is not a secret and is the useful half.
        assert!(shown.contains("eu-west-1"), "{shown}");
    }
}
