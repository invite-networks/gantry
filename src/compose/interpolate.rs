//! Compose-style variable interpolation (`$VAR`, `${VAR:-default}` and friends).

use crate::errors::{Error, Result};
use serde_norway::Value;

/// Looks up a variable. Returns `None` when it is unset.
pub trait Lookup {
    fn get(&self, name: &str) -> Option<String>;
}

impl<F: Fn(&str) -> Option<String>> Lookup for F {
    fn get(&self, name: &str) -> Option<String> {
        self(name)
    }
}

/// Interpolates every string scalar in a YAML tree. Mapping keys are left
/// alone, as Compose does.
pub fn interpolate_value(v: &mut Value, env: &dyn Lookup, path: &str) -> Result<()> {
    walk(v, env, path, true).map_err(|(p, e)| Error::config(format!("{p}: {e}")))
}

/// [`interpolate_value`] with [`interpolate_lenient`]. A failure names the
/// path of the value and the reason separately.
pub fn interpolate_value_lenient(
    v: &mut Value,
    env: &dyn Lookup,
    path: &str,
) -> std::result::Result<(), (String, String)> {
    walk(v, env, path, false)
}

fn walk(
    v: &mut Value,
    env: &dyn Lookup,
    path: &str,
    strict: bool,
) -> std::result::Result<(), (String, String)> {
    match v {
        Value::String(s) => {
            if s.contains('$') {
                *s = interpolate_at(s, env, 0, strict).map_err(|e| (path.to_string(), e))?;
            }
        }
        Value::Sequence(seq) => {
            for (i, item) in seq.iter_mut().enumerate() {
                walk(item, env, &format!("{path}[{i}]"), strict)?;
            }
        }
        Value::Mapping(map) => {
            for (k, item) in map.iter_mut() {
                let key = key_str(k).unwrap_or("?");
                let p = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                walk(item, env, &p, strict)?;
            }
        }
        Value::Tagged(t) => walk(&mut t.value, env, path, strict)?,
        _ => {}
    }
    Ok(())
}

/// A mapping key as text, seeing through a YAML tag on the key.
pub fn key_str(k: &Value) -> Option<&str> {
    match k {
        Value::Tagged(t) => key_str(&t.value),
        other => other.as_str(),
    }
}

/// One variable a string refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    /// False when the string needs a value for it: `$X`, `${X}`, `${X:?}`.
    pub has_default: bool,
}

/// Every variable `input` refers to, including those inside defaults and
/// messages, whether or not they would be evaluated.
pub fn references(input: &str) -> std::result::Result<Vec<Reference>, String> {
    let mut out = Vec::new();
    scan(input, 0, &mut out)?;
    Ok(out)
}

fn scan(input: &str, depth: usize, out: &mut Vec<Reference>) -> std::result::Result<(), String> {
    if depth > MAX_NESTING {
        return Err(format!("variables nested more than {MAX_NESTING} deep"));
    }
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' {
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('$') => i += 2,
            Some('{') => {
                let end = matching_brace(&chars, i + 1)
                    .ok_or_else(|| format!("unterminated variable in {input:?}"))?;
                let body: String = chars[i + 2..end].iter().collect();
                let name_end = body
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(body.len());
                let (name, rest) = body.split_at(name_end);
                if name.is_empty() {
                    return Err(format!("invalid variable reference ${{{body}}}"));
                }
                let (has_default, arg) = match rest {
                    "" => (false, ""),
                    r if r.starts_with(":-") || r.starts_with(":+") || r.starts_with(":?") => {
                        (!r.starts_with(":?"), &r[2..])
                    }
                    r if r.starts_with('-') || r.starts_with('+') || r.starts_with('?') => {
                        (!r.starts_with('?'), &r[1..])
                    }
                    _ => return Err(format!("invalid variable reference ${{{body}}}")),
                };
                out.push(Reference {
                    name: name.to_string(),
                    has_default,
                });
                scan(arg, depth + 1, out)?;
                i = end + 1;
            }
            Some(n) if n.is_ascii_alphabetic() || *n == '_' => {
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                out.push(Reference {
                    name: chars[start..j].iter().collect(),
                    has_default: false,
                });
                i = j;
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// How deeply `${...}` may nest inside a default, alternate or error
/// message. Real files nest one or two levels; the cap keeps hostile text
/// from exhausting the stack.
pub const MAX_NESTING: usize = 16;

/// Interpolates one string.
pub fn interpolate(input: &str, env: &dyn Lookup) -> std::result::Result<String, String> {
    interpolate_at(input, env, 0, true)
}

/// Like [`interpolate`], but a missing required variable (`${X:?}`)
/// becomes empty instead of an error. Used to check a file whose values
/// are not known.
pub fn interpolate_lenient(input: &str, env: &dyn Lookup) -> std::result::Result<String, String> {
    interpolate_at(input, env, 0, false)
}

fn interpolate_at(
    input: &str,
    env: &dyn Lookup,
    depth: usize,
    strict: bool,
) -> std::result::Result<String, String> {
    if depth > MAX_NESTING {
        return Err(format!("variables nested more than {MAX_NESTING} deep"));
    }
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '$' {
            out.push(c);
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('$') => {
                out.push('$');
                i += 2;
            }
            Some('{') => {
                let end = matching_brace(&chars, i + 1)
                    .ok_or_else(|| format!("unterminated variable in {input:?}"))?;
                let body: String = chars[i + 2..end].iter().collect();
                out.push_str(&expand_braced(&body, env, depth, strict)?);
                i = end + 1;
            }
            Some(n) if n.is_ascii_alphabetic() || *n == '_' => {
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let name: String = chars[start..j].iter().collect();
                out.push_str(&env.get(&name).unwrap_or_default());
                i = j;
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
    Ok(out)
}

fn matching_brace(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, c) in chars.iter().enumerate().skip(open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

fn expand_braced(
    body: &str,
    env: &dyn Lookup,
    depth: usize,
    strict: bool,
) -> std::result::Result<String, String> {
    let name_end = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(body.len());
    let name = &body[..name_end];
    if name.is_empty() {
        return Err(format!("invalid variable reference ${{{body}}}"));
    }
    let rest = &body[name_end..];
    let value = env.get(name);
    let (op, arg) = if let Some(a) = rest.strip_prefix(":-") {
        (":-", a)
    } else if let Some(a) = rest.strip_prefix(":?") {
        (":?", a)
    } else if let Some(a) = rest.strip_prefix(":+") {
        (":+", a)
    } else if let Some(a) = rest.strip_prefix('-') {
        ("-", a)
    } else if let Some(a) = rest.strip_prefix('?') {
        ("?", a)
    } else if let Some(a) = rest.strip_prefix('+') {
        ("+", a)
    } else if rest.is_empty() {
        ("", "")
    } else {
        return Err(format!("invalid variable reference ${{{body}}}"));
    };
    let set_nonempty = value.as_deref().is_some_and(|v| !v.is_empty());
    Ok(match op {
        "" => value.unwrap_or_default(),
        ":-" if set_nonempty => value.unwrap_or_default(),
        ":-" => interpolate_at(arg, env, depth + 1, strict)?,
        "-" if value.is_some() => value.unwrap_or_default(),
        "-" => interpolate_at(arg, env, depth + 1, strict)?,
        ":?" if set_nonempty => value.unwrap_or_default(),
        "?" if value.is_some() => value.unwrap_or_default(),
        ":?" | "?" if !strict => String::new(),
        ":?" | "?" => {
            let msg = interpolate_at(arg, env, depth + 1, strict)?;
            return Err(if msg.is_empty() {
                format!("required variable {name} is missing a value")
            } else {
                format!("required variable {name} is missing a value: {msg}")
            });
        }
        ":+" if set_nonempty => interpolate_at(arg, env, depth + 1, strict)?,
        "+" if value.is_some() => interpolate_at(arg, env, depth + 1, strict)?,
        _ => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env() -> impl Fn(&str) -> Option<String> {
        let m: HashMap<&str, &str> = [("A", "apple"), ("EMPTY", ""), ("N", "3")].into();
        move |k: &str| m.get(k).map(|v| v.to_string())
    }

    #[test]
    fn plain_and_braced() {
        let e = env();
        assert_eq!(interpolate("x $A y ${A}", &e).unwrap(), "x apple y apple");
        assert_eq!(interpolate("$MISSING-", &e).unwrap(), "-");
        assert_eq!(interpolate("cost $$5", &e).unwrap(), "cost $5");
        assert_eq!(interpolate("tail $", &e).unwrap(), "tail $");
    }

    #[test]
    fn defaults() {
        let e = env();
        assert_eq!(interpolate("${MISSING:-def}", &e).unwrap(), "def");
        assert_eq!(interpolate("${EMPTY:-def}", &e).unwrap(), "def");
        assert_eq!(interpolate("${EMPTY-def}", &e).unwrap(), "");
        assert_eq!(interpolate("${MISSING-def}", &e).unwrap(), "def");
        assert_eq!(interpolate("${MISSING:-${A}x}", &e).unwrap(), "applex");
        assert_eq!(interpolate("${A:+alt}", &e).unwrap(), "alt");
        assert_eq!(interpolate("${EMPTY:+alt}", &e).unwrap(), "");
        assert_eq!(interpolate("${EMPTY+alt}", &e).unwrap(), "alt");
    }

    #[test]
    fn required() {
        let e = env();
        let err = interpolate("${MISSING:?set it}", &e).unwrap_err();
        assert!(err.contains("MISSING") && err.contains("set it"));
        assert!(interpolate("${EMPTY?x}", &e).is_ok());
        assert!(interpolate("${EMPTY:?x}", &e).is_err());
    }

    #[test]
    fn references_and_lenient() {
        let r = references("$A ${B} ${C:-x${D}} ${E:?need ${F:-y}} ${G:+z} $$H").unwrap();
        let got: Vec<(&str, bool)> = r.iter().map(|r| (r.name.as_str(), r.has_default)).collect();
        assert_eq!(
            got,
            [
                ("A", false),
                ("B", false),
                ("C", true),
                ("D", false),
                ("E", false),
                ("F", true),
                ("G", true)
            ]
        );
        let none = |_: &str| None;
        assert_eq!(interpolate_lenient("a${E:?must}b", &none).unwrap(), "ab");
        assert!(interpolate("a${E:?must}b", &none).is_err());
    }

    #[test]
    fn nesting_is_capped() {
        let e = env();
        let nested = |n: usize| format!("{}x{}", "${NOPE:-".repeat(n), "}".repeat(n));
        assert_eq!(interpolate(&nested(MAX_NESTING), &e).unwrap(), "x");
        assert!(
            interpolate(&nested(100_000), &e)
                .unwrap_err()
                .contains("nested")
        );
    }

    #[test]
    fn invalid() {
        let e = env();
        assert!(interpolate("${A", &e).is_err());
        assert!(interpolate("${}", &e).is_err());
        assert!(interpolate("${A!x}", &e).is_err());
    }
}
