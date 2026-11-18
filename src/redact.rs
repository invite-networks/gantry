//! Hides likely secrets when a project is printed (`gantry config`).
//!
//! A value is replaced when its variable name looks sensitive (see
//! [`sensitive_name`]), and passwords inside URLs such as
//! `postgres://app:hunter2@db/app` are replaced wherever they appear. The
//! project used to run containers is never changed, only the copy printed.

use indexmap::IndexMap;

use crate::project::Project;

/// What a hidden value is printed as.
pub const REDACTED: &str = "<redacted>";

/// Name parts that mark a whole value as secret. Names are split on
/// anything that is not a letter or digit, so `API_KEY` matches `KEY` but
/// `KEYCLOAK_URL` does not.
const SECRET_WORDS: &[&str] = &[
    "KEY",
    "KEYS",
    "PASSWORD",
    "PASSWD",
    "PASS",
    "PWD",
    "SECRET",
    "SECRETS",
    "TOKEN",
    "TOKENS",
    "CREDENTIAL",
    "CREDENTIALS",
    "CREDS",
    "AUTH",
    "PRIVATE",
    "SALT",
    "DSN",
    "COOKIE",
    "SESSION",
    "SIGNATURE",
];

/// Substrings that mark a name as secret even when run together with other
/// words, such as `APIKEY` or `DBPASSWORD`.
const SECRET_PARTS: &[&str] = &[
    "PASSWORD",
    "PASSWD",
    "SECRET",
    "TOKEN",
    "APIKEY",
    "PRIVATEKEY",
    "ACCESSKEY",
    "CREDENTIAL",
];

/// Returns true when a variable name looks like it holds a secret.
///
/// Names ending in `_FILE` are left alone: by convention they hold the path
/// to a secret file (`POSTGRES_PASSWORD_FILE=/run/secrets/db`), not the
/// secret itself.
pub fn sensitive_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if upper.ends_with("_FILE") {
        return false;
    }
    upper
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| SECRET_WORDS.contains(&w))
        || SECRET_PARTS.iter().any(|p| upper.contains(p))
}

/// Replaces the password in every `scheme://user:password@host` in `value`.
pub fn redact_url_passwords(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        // The authority ends at the first '/', '?', '#' or whitespace.
        let end = tail
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                let userinfo = &authority[..at];
                match userinfo.find(':') {
                    Some(colon) => {
                        out.push_str(&userinfo[..=colon]);
                        out.push_str(REDACTED);
                        out.push_str(&authority[at..]);
                    }
                    None => out.push_str(authority),
                }
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn redact_map(map: &mut IndexMap<String, String>) {
    for (name, value) in map.iter_mut() {
        if value.is_empty() {
            continue;
        }
        *value = if sensitive_name(name) {
            REDACTED.to_string()
        } else {
            redact_url_passwords(value)
        };
    }
}

/// A copy of `project` with likely secrets in service environments and build
/// arguments replaced by [`REDACTED`].
pub fn redacted(project: &Project) -> Project {
    let mut p = project.clone();
    for svc in p.services.values_mut() {
        redact_map(&mut svc.environment);
        if let Some(b) = svc.build.as_mut() {
            redact_map(&mut b.args);
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_names() {
        for n in [
            "API_KEY",
            "api_key",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_ACCESS_KEY_ID",
            "POSTGRES_PASSWORD",
            "DB_PASS",
            "MYSQL_ROOT_PWD",
            "GITHUB_TOKEN",
            "SECRET_KEY_BASE",
            "APIKEY",
            "DBPASSWORD",
            "OAUTH_CLIENT_SECRET",
            "SENTRY_DSN",
            "BASIC_AUTH",
            "TLS_PRIVATE_KEY",
            "SESSION_SALT",
            "smtp.password",
        ] {
            assert!(sensitive_name(n), "{n} should be redacted");
        }
    }

    #[test]
    fn ordinary_names() {
        for n in [
            "KEYCLOAK_URL",
            "MONKEY_MODE",
            "PASSIVE",
            "AUTHOR",
            "TZ",
            "LOG_LEVEL",
            "POSTGRES_USER",
            "POSTGRES_PASSWORD_FILE",
            "DATABASE_HOST",
        ] {
            assert!(!sensitive_name(n), "{n} should be shown");
        }
    }

    #[test]
    fn url_passwords() {
        assert_eq!(
            redact_url_passwords("postgres://app:hunter2@db:5432/app?sslmode=off"),
            "postgres://app:<redacted>@db:5432/app?sslmode=off"
        );
        assert_eq!(
            redact_url_passwords("redis://:s3cret@cache:6379/0"),
            "redis://:<redacted>@cache:6379/0"
        );
        assert_eq!(
            redact_url_passwords("a=https://u:p@x/ b=amqp://v:q@y"),
            "a=https://u:<redacted>@x/ b=amqp://v:<redacted>@y"
        );
        // Passwords containing '@' are hidden up to the last '@'.
        assert_eq!(
            redact_url_passwords("https://u:p@ss@host/"),
            "https://u:<redacted>@host/"
        );
        for s in [
            "https://example.com/path",
            "https://user@example.com/",
            "plain value",
            "",
        ] {
            assert_eq!(redact_url_passwords(s), s);
        }
    }

    #[test]
    fn redact_map_values() {
        let mut m: IndexMap<String, String> = [
            ("API_KEY", "abc"),
            ("EMPTY_TOKEN", ""),
            ("DATABASE_URL", "postgres://a:b@db/x"),
            ("TZ", "UTC"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        redact_map(&mut m);
        assert_eq!(m["API_KEY"], REDACTED);
        assert_eq!(m["EMPTY_TOKEN"], "");
        assert_eq!(m["DATABASE_URL"], "postgres://a:<redacted>@db/x");
        assert_eq!(m["TZ"], "UTC");
    }
}
