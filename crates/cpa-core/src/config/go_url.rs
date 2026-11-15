//! What Go 1.26 `net/url.Parse` accepts (strict host colons, as CLIProxyAPI's
//! `go 1.26.0` module selects), plus `URL.Scheme` and `URL.Hostname()`. Config and
//! credential checks use it where Go branches on a parse error.

use std::net::{Ipv4Addr, Ipv6Addr};

pub struct GoUrl {
    /// Lowercased.
    pub scheme: String,
    /// The unescaped authority host with its port, when the URL has `//authority`.
    host: Option<String>,
}

impl GoUrl {
    /// Go `URL.Host`: the unescaped authority host with its port; empty without one.
    pub fn host(&self) -> &str {
        self.host.as_deref().unwrap_or_default()
    }

    /// Go `URL.Hostname()`.
    pub fn hostname(&self) -> &str {
        let mut host = self.host.as_deref().unwrap_or_default();
        if let Some(colon) = host.rfind(':')
            && valid_optional_port(&host[colon..])
        {
            host = &host[..colon];
        }
        host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
    }
}

/// Go `url.Parse`; `None` wherever Go returns an error.
pub fn parse(raw: &str) -> Option<GoUrl> {
    let (u, frag) = raw.split_once('#').unwrap_or((raw, ""));
    if u.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let mut url = GoUrl {
        scheme: String::new(),
        host: None,
    };
    if u == "*" {
        return (frag.is_empty() || unescape(frag, Mode::Other).is_some()).then_some(url);
    }
    let (scheme, rest) = get_scheme(u)?;
    url.scheme = scheme.to_ascii_lowercase();
    let mut rest = if rest.ends_with('?') && rest.matches('?').count() == 1 {
        &rest[..rest.len() - 1]
    } else {
        rest.split_once('?').map_or(rest, |(r, _)| r)
    };
    if !rest.starts_with('/') {
        if !url.scheme.is_empty() {
            // Opaque: Go validates nothing else but the fragment.
            return (frag.is_empty() || unescape(frag, Mode::Other).is_some()).then_some(url);
        }
        if rest.split('/').next().unwrap_or_default().contains(':') {
            return None;
        }
    }
    if (!url.scheme.is_empty() || !rest.starts_with("///")) && rest.starts_with("//") {
        let authority = &rest[2..];
        let (authority, path) = authority.find('/').map_or((authority, ""), |i| authority.split_at(i));
        url.host = Some(parse_authority(&url.scheme, authority)?);
        rest = path;
    }
    unescape(rest, Mode::Other)?;
    if !frag.is_empty() {
        unescape(frag, Mode::Other)?;
    }
    Some(url)
}

/// Go `URL.RequestURI()` after `url.Parse(raw)` for a URL with a path (`EscapedPath`,
/// then `?` and the raw query); `None` where Parse fails or the URL is opaque.
pub fn request_uri(raw: &str) -> Option<String> {
    let url = Escaped::parse(raw)?;
    (!url.opaque && (url.path.is_empty() || url.path.starts_with('/'))).then(|| url.request_uri())
}

/// A URL Go's `url.Parse` accepts, held as Go escapes it on the way out.
#[derive(Clone, Debug, PartialEq)]
pub struct Escaped {
    /// Lowercased.
    scheme: String,
    /// The user info as written, without its `@`.
    user: Option<String>,
    /// The host and port as written; empty without one.
    host: String,
    /// `URL.Opaque` is set; `path` is then empty.
    opaque: bool,
    /// `URL.EscapedPath()`.
    path: String,
    /// `URL.RawQuery`; `Some("")` with `ForceQuery`.
    query: Option<String>,
    /// `URL.EscapedFragment()`; empty without one.
    fragment: String,
}

impl Escaped {
    /// Go `url.Parse`; `None` wherever Go returns an error.
    pub fn parse(raw: &str) -> Option<Self> {
        parse(raw)?;
        let (u, frag) = raw.split_once('#').unwrap_or((raw, ""));
        let (scheme, rest) = get_scheme(u)?;
        let (rest, query) = if rest.ends_with('?') && rest.matches('?').count() == 1 {
            (&rest[..rest.len() - 1], Some(String::new()))
        } else {
            rest.split_once('?')
                .map_or((rest, None), |(r, q)| (r, Some(q.to_owned())))
        };
        let mut url = Escaped {
            scheme: scheme.to_ascii_lowercase(),
            user: None,
            host: String::new(),
            opaque: false,
            path: String::new(),
            query,
            fragment: escaped(frag, fragment_should_escape)?,
        };
        if !rest.starts_with('/') && !scheme.is_empty() {
            url.opaque = true;
            return Some(url);
        }
        let mut path = rest;
        if (!scheme.is_empty() || !rest.starts_with("///"))
            && let Some(authority) = rest.strip_prefix("//")
        {
            let (authority, p) = authority.split_at(authority.find('/').unwrap_or(authority.len()));
            match authority.rsplit_once('@') {
                Some((user, host)) => {
                    url.user = Some(user.to_owned());
                    url.host = host.to_owned();
                }
                None => url.host = authority.to_owned(),
            }
            path = p;
        }
        url.path = escaped(path, path_should_escape)?;
        Some(url)
    }

    /// Go `u.Parse(reference)`: `url.Parse`, then `URL.ResolveReference`.
    pub fn join(&self, reference: &str) -> Option<Self> {
        let r = Self::parse(reference)?;
        let mut url = r.clone();
        if r.scheme.is_empty() {
            url.scheme.clone_from(&self.scheme);
        }
        if !r.scheme.is_empty() || !r.host.is_empty() || r.user.is_some() {
            url.path = resolve_path(&r.path, "");
            return Some(url);
        }
        if r.path.is_empty() && r.query.is_none() {
            // Only `RawQuery` is inherited; `ForceQuery` (a bare `?`) stays the reference's.
            url.query = self.query.clone().filter(|q| !q.is_empty());
            if r.fragment.is_empty() {
                url.fragment.clone_from(&self.fragment);
            }
        }
        if r.path.is_empty() && self.opaque {
            url.opaque = true;
            url.user = None;
            url.host.clear();
            url.path.clear();
            return Some(url);
        }
        url.host.clone_from(&self.host);
        url.user.clone_from(&self.user);
        url.path = resolve_path(&self.path, &r.path);
        Some(url)
    }

    /// Lowercased.
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Go `URL.Host` as written; empty for an opaque URL or one without an authority.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Go `removeEmptyPort`, which `http.NewRequest` applies to the first hop's host.
    pub fn remove_empty_port(&mut self) {
        if self.host.ends_with(':') {
            self.host.pop();
        }
    }

    /// `username:password` with both unescaped, as Go's client joins `URL.User` for
    /// Basic auth (an absent password is empty); `None` without user info.
    pub fn basic_credentials(&self) -> Option<Vec<u8>> {
        let user = self.user.as_deref()?;
        let (name, password) = user.split_once(':').unwrap_or((user, ""));
        let mut out = unescape_bytes(name)?;
        out.push(b':');
        out.extend(unescape_bytes(password)?);
        Some(out)
    }

    /// Go `URL.RequestURI()` for a URL that is not opaque.
    pub fn request_uri(&self) -> String {
        let mut target = if self.path.is_empty() {
            "/".to_owned()
        } else {
            self.path.clone()
        };
        if let Some(query) = &self.query {
            target.push('?');
            target.push_str(query);
        }
        target
    }

    /// Go `URL.String()` with the user info removed: the `Referer` Go's client sends
    /// from this URL.
    pub fn referer(&self) -> String {
        let mut out = format!("{}://{}{}", self.scheme, self.host, self.path);
        if let Some(query) = &self.query {
            out.push('?');
            out.push_str(query);
        }
        if !self.fragment.is_empty() {
            out.push('#');
            out.push_str(&self.fragment);
        }
        out
    }
}

/// Go `net/url.resolvePath`: merge `reference` with `base`, then remove dot segments.
pub fn resolve_path(base: &str, reference: &str) -> String {
    let full = if reference.is_empty() {
        base.to_owned()
    } else if !reference.starts_with('/') {
        let i = base.rfind('/').map_or(0, |i| i + 1);
        format!("{}{reference}", &base[..i])
    } else {
        reference.to_owned()
    };
    if full.is_empty() {
        return String::new();
    }
    let mut dst = String::with_capacity(full.len() + 1);
    dst.push('/');
    let mut first = true;
    let mut last = "";
    for elem in full.split('/') {
        last = elem;
        match elem {
            "." => first = false,
            ".." => {
                match dst[1..].rfind('/') {
                    Some(i) => dst.truncate(i + 1),
                    None => dst.truncate(1),
                }
                first = dst.len() == 1;
            }
            _ => {
                if !first {
                    dst.push('/');
                }
                dst.push_str(elem);
                first = false;
            }
        }
    }
    if last == "." || last == ".." {
        dst.push('/');
    }
    // An initial `/` was written; never two.
    if dst.len() > 1 && dst.as_bytes()[1] == b'/' {
        dst.remove(0);
    }
    dst
}

/// Go `shouldEscape(c, encodePath)`.
fn path_should_escape(c: u8) -> bool {
    !(c.is_ascii_alphanumeric() || b"-_.~$&+,/:;=@".contains(&c))
}

/// Go `shouldEscape(c, encodeFragment)`.
fn fragment_should_escape(c: u8) -> bool {
    !(c.is_ascii_alphanumeric() || b"-_.~$&+,/:;=?@!()*".contains(&c))
}

/// Go's `EscapedPath` and `EscapedFragment` over `url.Parse`'s input: as written when
/// `validEncoded`, else Go's `escape` of the unescaped bytes.
fn escaped(s: &str, should_escape: fn(u8) -> bool) -> Option<String> {
    if s.bytes().all(|c| b"!$&'()*+,;=:@[]%".contains(&c) || !should_escape(c)) {
        return Some(s.to_owned());
    }
    let mut out = String::with_capacity(s.len());
    for c in unescape_bytes(s)? {
        if should_escape(c) {
            out.push_str(&format!("%{c:02X}"));
        } else {
            out.push(c as char);
        }
    }
    Some(out)
}

/// Go `unescape(s, encodePath)` as bytes.
fn unescape_bytes(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(hex(*b.get(i + 1)?)? << 4 | hex(*b.get(i + 2)?)?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

fn get_scheme(raw: &str) -> Option<(&str, &str)> {
    for (i, c) in raw.bytes().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' if i > 0 => {}
            b':' if i == 0 => return None,
            b':' => return Some((&raw[..i], &raw[i + 1..])),
            _ => return Some(("", raw)),
        }
    }
    Some(("", raw))
}

fn parse_authority(scheme: &str, authority: &str) -> Option<String> {
    let (userinfo, host) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    let host = parse_host(scheme, host)?;
    if let Some(userinfo) = userinfo {
        let valid = userinfo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._:~!$&'()*+,;=%@".contains(c));
        if !valid {
            return None;
        }
        for part in userinfo.splitn(2, ':') {
            unescape(part, Mode::Other)?;
        }
    }
    Some(host)
}

fn parse_host(scheme: &str, host: &str) -> Option<String> {
    if let Some(open) = host.rfind('[') {
        // Go 1.26.4: an IP literal must start the host ("invalid IP-literal").
        if open != 0 {
            return None;
        }
        let close = host.rfind(']')?;
        let colon_port = &host[close + 1..];
        if !valid_optional_port(colon_port) {
            return None;
        }
        let inner = host.get(open + 1..close)?;
        let unescaped = match inner.find("%25") {
            Some(zone) => unescape(&inner[..zone], Mode::Host)? + &unescape(&inner[zone..], Mode::Zone)?,
            None => unescape(inner, Mode::Host)?,
        };
        // netip.ParseAddr: an IPv6 address, optionally with a non-empty zone after the
        // first '%'; never plain IPv4.
        let (addr, zone) = unescaped
            .split_once('%')
            .map_or((unescaped.as_str(), None), |(a, z)| (a, Some(z)));
        if zone == Some("") || addr.parse::<Ipv6Addr>().is_err() || addr.parse::<Ipv4Addr>().is_ok() {
            return None;
        }
        return Some(format!("[{unescaped}]{colon_port}"));
    }
    if let Some(i) = host.find(':') {
        let i = if host.rfind(':') != Some(i) && matches!(scheme, "postgres" | "postgresql") {
            host.rfind(':').unwrap_or(i)
        } else {
            i
        };
        if !valid_optional_port(&host[i..]) {
            return None;
        }
    }
    unescape(host, Mode::Host)
}

fn valid_optional_port(port: &str) -> bool {
    port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(PartialEq)]
enum Mode {
    Host,
    Zone,
    Other,
}

fn host_byte_ok(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!\"$&'()*+,-.:;<=>[]_~".contains(&b)
}

/// Go `unescape` validation and decoding for the modes `Parse` uses.
fn unescape(s: &str, mode: Mode) -> Option<String> {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let (hi, lo) = (hex(*b.get(i + 1)?)?, hex(*b.get(i + 2)?)?);
            let is_25 = &b[i..i + 3] == b"%25";
            if mode == Mode::Host && hi < 8 && !is_25 {
                return None;
            }
            let v = hi << 4 | lo;
            if mode == Mode::Zone && !is_25 && v != b' ' && !host_byte_ok(v) {
                return None;
            }
            out.push(v);
            i += 3;
            continue;
        }
        if mode != Mode::Other && b[i] < 0x80 && !host_byte_ok(b[i]) {
            return None;
        }
        out.push(b[i]);
        i += 1;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Endpoint parsing protects supported HTTP authorities rather than arbitrary opaque URL schemes.
    #[test]
    fn http_endpoints_preserve_authority_and_reject_ambiguous_hosts() {
        for (raw, scheme, hostname) in [
            ("https://API.example.invalid:8443/v1", "https", "API.example.invalid"),
            ("http://127.0.0.1:8317/v1", "http", "127.0.0.1"),
            ("https://[::1]:8080/v1", "https", "::1"),
            ("http://[fe80::1%25eth0]/", "http", "fe80::1%eth0"),
        ] {
            let url = parse(raw).unwrap();
            assert_eq!(url.scheme, scheme);
            assert_eq!(url.hostname(), hostname);
        }
        for invalid in [
            "https://api.example.invalid/%zz",
            "https://h:abc/v1",
            "https://a:1:2/",
            "https://[1.2.3.4]/",
            "http://x[::1]",
            "http://x[::1]:80/p",
            "https://[::1/",
            "https://[::1%25]/",
            "https://a b/",
            "https://a%41/",
            "https://u{@h/",
            "http://h/\x01",
        ] {
            assert!(parse(invalid).is_none(), "{invalid}");
        }
    }

    /// HTTP request targets retain escaped path delimiters and exclude fragments.
    #[test]
    fn request_targets_escape_paths_and_preserve_existing_escapes() {
        for (raw, expected) in [
            ("http://h/my catalogs/models.json", "/my%20catalogs/models.json"),
            ("https://h/modèles/models.json", "/mod%C3%A8les/models.json"),
            ("https://h/a%2Fb/x.json", "/a%2Fb/x.json"),
            ("https://h/a/../c.json?x=1&y=a%20b", "/a/../c.json?x=1&y=a%20b"),
            ("https://h", "/"),
            ("https://h?", "/?"),
            ("https://h/p#fragment", "/p"),
            ("https://h/%41", "/%41"),
        ] {
            assert_eq!(request_uri(raw).as_deref(), Some(expected), "{raw}");
            assert!(!expected.contains('#'));
        }
        assert_eq!(request_uri("https://h/%zz"), None);
        assert_eq!(request_uri("mailto:x"), None);
    }

    /// Normalized endpoint references retain explicit ports and never expose URL credentials.
    #[test]
    fn endpoint_normalization_omits_empty_ports_and_redacts_userinfo() {
        for (raw, expected) in [
            ("http://h:/x", "http://h/x"),
            ("http://[::1]:/x?", "http://[::1]/x?"),
            ("http://secret-user:secret-password@h:/x", "http://h/x"),
            ("http://h:80/x", "http://h:80/x"),
        ] {
            let mut url = Escaped::parse(raw).unwrap();
            url.remove_empty_port();
            let rendered = url.referer();
            assert_eq!(rendered, expected);
            assert!(!rendered.contains("secret-user") && !rendered.contains("secret-password"));
        }
    }

    /// Relative URL path resolution follows RFC 3986 examples, including traversal above the root.
    #[test]
    fn relative_paths_remove_dot_segments_without_escaping_root() {
        // RFC 3986 section 5.4 relative-reference examples for base /b/c/d;p.
        for (reference, expected) in [
            ("g", "/b/c/g"),
            ("./g", "/b/c/g"),
            ("../g", "/b/g"),
            ("../../g", "/g"),
            ("../../../g", "/g"),
            (".", "/b/c/"),
            ("..", "/b/"),
        ] {
            assert_eq!(resolve_path("/b/c/d;p", reference), expected);
        }
        assert_eq!(resolve_path("//api/item", "next"), "//api/next");
        assert_eq!(resolve_path("", "next"), "/next");
        assert_eq!(resolve_path("", ""), "");
    }
}
