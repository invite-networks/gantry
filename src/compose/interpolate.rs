//! Compose-style variable interpolation (`$VAR`, `${VAR:-default}` and friends).

use crate::errors::{Error, Result};
use serde_yaml_ng::Value;

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
    match v {
        Value::String(s) => {
            if s.contains('$') {
                *s = interpolate(s, env).map_err(|e| Error::config(format!("{path}: {e}")))?;
            }
        }
        Value::Sequence(seq) => {
            for (i, item) in seq.iter_mut().enumerate() {
                interpolate_value(item, env, &format!("{path}[{i}]"))?;
            }
        }
        Value::Mapping(map) => {
            for (k, item) in map.iter_mut() {
                let key = k.as_str().unwrap_or("?");
                interpolate_value(item, env, &format!("{path}.{key}"))?;
            }
        }
        Value::Tagged(t) => interpolate_value(&mut t.value, env, path)?,
        _ => {}
    }
    Ok(())
}

/// Interpolates one string.
pub fn interpolate(input: &str, env: &dyn Lookup) -> std::result::Result<String, String> {
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
                out.push_str(&expand_braced(&body, env)?);
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

fn expand_braced(body: &str, env: &dyn Lookup) -> std::result::Result<String, String> {
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
        ":-" => interpolate(arg, env)?,
        "-" if value.is_some() => value.unwrap_or_default(),
        "-" => interpolate(arg, env)?,
        ":?" if set_nonempty => value.unwrap_or_default(),
        "?" if value.is_some() => value.unwrap_or_default(),
        ":?" | "?" => {
            let msg = interpolate(arg, env)?;
            return Err(if msg.is_empty() {
                format!("required variable {name} is missing a value")
            } else {
                format!("required variable {name} is missing a value: {msg}")
            });
        }
        ":+" if set_nonempty => interpolate(arg, env)?,
        "+" if value.is_some() => interpolate(arg, env)?,
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
    fn invalid() {
        let e = env();
        assert!(interpolate("${A", &e).is_err());
        assert!(interpolate("${}", &e).is_err());
        assert!(interpolate("${A!x}", &e).is_err());
    }
}
