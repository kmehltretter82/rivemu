//! QEMU-style `key=value,...` option strings (the `QemuOpts` grammar).

use thiserror::Error;

/// Error parsing an option string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OptsError {
    /// An empty parameter name, e.g. `=value` or `a=1,,=2`.
    #[error("invalid parameter '{0}': empty name")]
    EmptyName(String),
}

/// A parsed option string such as `user,id=net0,hostfwd=tcp::2222-:22`.
///
/// Grammar (as in QEMU):
/// * parameters are separated by `,`; a literal comma is written `,,`
/// * `key=value` sets a parameter
/// * a bare first item is the value of the option's *implied* key
///   (e.g. `type` for `-netdev`, `driver` for `-device`)
/// * any other bare item `key` means `key=on`
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QemuOpts {
    params: Vec<(String, String)>,
}

impl QemuOpts {
    /// Parses `s`, using `implied_key` for a bare first item.
    pub fn parse(s: &str, implied_key: Option<&str>) -> Result<Self, OptsError> {
        let mut params = Vec::new();
        for (i, item) in split_items(s).into_iter().enumerate() {
            if item.is_empty() {
                continue;
            }
            match item.split_once('=') {
                Some(("", _)) => return Err(OptsError::EmptyName(item)),
                Some((k, v)) => params.push((k.to_owned(), v.to_owned())),
                None => match implied_key {
                    Some(key) if i == 0 => params.push((key.to_owned(), item)),
                    _ => params.push((item, "on".to_owned())),
                },
            }
        }
        Ok(Self { params })
    }

    /// Last value given for `key` (QEMU semantics: later values win).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// All parameters in order.
    pub fn params(&self) -> &[(String, String)] {
        &self.params
    }
}

/// Splits on `,` while treating `,,` as an escaped comma.
fn split_items(s: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ',' {
            if chars.peek() == Some(&',') {
                chars.next();
                cur.push(',');
            } else {
                items.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    items.push(cur);
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str, implied: Option<&str>) -> Vec<(String, String)> {
        QemuOpts::parse(s, implied).unwrap().params
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn implied_key() {
        assert_eq!(
            p("user,id=net0,hostfwd=tcp::2222-:22", Some("type")),
            kv(&[
                ("type", "user"),
                ("id", "net0"),
                ("hostfwd", "tcp::2222-:22")
            ])
        );
        assert_eq!(p("id=x", Some("type")), kv(&[("id", "x")]));
    }

    #[test]
    fn bare_flags_mean_on() {
        assert_eq!(
            p("virt,aclint,dumpdtb=a.dtb", Some("type")),
            kv(&[("type", "virt"), ("aclint", "on"), ("dumpdtb", "a.dtb")])
        );
        assert_eq!(p("readonly", None), kv(&[("readonly", "on")]));
    }

    #[test]
    fn escaped_commas_and_values_with_equals() {
        assert_eq!(
            p("file=a,,b.img,opt=x=y", None),
            kv(&[("file", "a,b.img"), ("opt", "x=y")])
        );
    }

    #[test]
    fn empty_items_ignored_and_last_wins() {
        let o = QemuOpts::parse("a=1,,,a=2,", None).unwrap();
        // "a=1,,,a=2," -> "a=1," then "a=2" then ""
        assert_eq!(o.params(), kv(&[("a", "1,"), ("a", "2")]).as_slice());
        assert_eq!(o.get("a"), Some("2"));
        assert_eq!(o.get("b"), None);
    }

    #[test]
    fn empty_name_rejected() {
        assert!(QemuOpts::parse("=x", None).is_err());
    }
}
