//! Minimal RFC 3986-ish URL splitting, matching Python's `urllib.parse.urlsplit`
//! closely enough for capability URLs.
//!
//! The `url` crate normalizes URLs (adds trailing slashes, lowercases hosts),
//! which would change strings that must round-trip byte-for-byte through the
//! manifest. This splitter only takes URLs apart and puts them back together.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum UrlError {
    #[error("invalid URL (no scheme): {0:?}")]
    NoScheme(String),
    #[error("invalid URL (no host): {0:?}")]
    NoHost(String),
}

/// Split components of a URL: `scheme://[user@]host[:port][path][?query][#fragment]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    pub scheme: String,
    pub userinfo: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

impl UrlParts {
    pub fn parse(url: &str) -> Result<UrlParts, UrlError> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| UrlError::NoScheme(url.to_string()))?;
        if scheme.is_empty() {
            return Err(UrlError::NoScheme(url.to_string()));
        }

        let (rest, fragment) = match rest.split_once('#') {
            Some((r, f)) => (r, Some(f.to_string())),
            None => (rest, None),
        };
        let (rest, query) = match rest.split_once('?') {
            Some((r, q)) => (r, Some(q.to_string())),
            None => (rest, None),
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, String::new()),
        };

        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h),
            None => (None, authority),
        };

        let (host, port) = if let Some(bracketed) = hostport.strip_prefix('[') {
            // IPv6 literal: [::1]:8080
            match bracketed.split_once(']') {
                Some((h, rest)) => {
                    let port = rest
                        .strip_prefix(':')
                        .map(str::parse)
                        .transpose()
                        .ok()
                        .flatten();
                    (h.to_string(), port)
                }
                None => (hostport.to_string(), None),
            }
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => match p.parse::<u16>() {
                    Ok(port) => (h.to_string(), Some(port)),
                    Err(_) => (hostport.to_string(), None),
                },
                None => (hostport.to_string(), None),
            }
        };
        if host.is_empty() {
            return Err(UrlError::NoHost(url.to_string()));
        }

        Ok(UrlParts {
            scheme: scheme.to_string(),
            userinfo,
            host,
            port,
            path,
            query,
            fragment,
        })
    }

    /// Is the host a loopback address (`127.0.0.1`, `localhost`, `::1`)?
    pub fn is_loopback(&self) -> bool {
        matches!(self.host.as_str(), "127.0.0.1" | "localhost" | "::1")
    }

    /// Reassemble the URL, optionally overriding host and port (preserves userinfo).
    pub fn with_address(&self, host: &str, port: u16) -> String {
        let mut parts = self.clone();
        parts.host = host.to_string();
        parts.port = Some(port);
        parts.to_url()
    }

    pub fn to_url(&self) -> String {
        let mut out = format!("{}://", self.scheme);
        if let Some(user) = &self.userinfo {
            out.push_str(user);
            out.push('@');
        }
        if self.host.contains(':') {
            out.push('[');
            out.push_str(&self.host);
            out.push(']');
        } else {
            out.push_str(&self.host);
        }
        if let Some(port) = self.port {
            out.push_str(&format!(":{port}"));
        }
        out.push_str(&self.path);
        if let Some(query) = &self.query {
            out.push('?');
            out.push_str(query);
        }
        if let Some(fragment) = &self.fragment {
            out.push('#');
            out.push_str(fragment);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_full_url() {
        let parts = UrlParts::parse("ssh://agent@10.0.0.1:2222/work?a=1#frag").unwrap();
        assert_eq!(parts.scheme, "ssh");
        assert_eq!(parts.userinfo.as_deref(), Some("agent"));
        assert_eq!(parts.host, "10.0.0.1");
        assert_eq!(parts.port, Some(2222));
        assert_eq!(parts.path, "/work");
        assert_eq!(parts.query.as_deref(), Some("a=1"));
        assert_eq!(parts.fragment.as_deref(), Some("frag"));
        assert_eq!(parts.to_url(), "ssh://agent@10.0.0.1:2222/work?a=1#frag");
    }

    #[test]
    fn rewrites_address_preserving_userinfo() {
        let parts = UrlParts::parse("ssh://agent@127.0.0.1:2222").unwrap();
        assert!(parts.is_loopback());
        assert_eq!(
            parts.with_address("127.0.0.1", 9999),
            "ssh://agent@127.0.0.1:9999"
        );
    }

    #[test]
    fn ipv6_host() {
        let parts = UrlParts::parse("tcp://[::1]:8080").unwrap();
        assert_eq!(parts.host, "::1");
        assert_eq!(parts.port, Some(8080));
        assert!(parts.is_loopback());
        assert_eq!(parts.to_url(), "tcp://[::1]:8080");
    }

    #[test]
    fn no_port_roundtrip() {
        let parts = UrlParts::parse("ws://example.com/mcp").unwrap();
        assert_eq!(parts.port, None);
        assert_eq!(parts.to_url(), "ws://example.com/mcp");
    }
}
