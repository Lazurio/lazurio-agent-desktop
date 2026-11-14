//! What people type as "the address of Lazurio" and what the app keeps of it.

use crate::{Error, Result};
use url::{Host, Url};

/// Turns `launchpad.example.com`, `https://launchpad.example.com/files` and the
/// like into the origin `https://launchpad.example.com`.
///
/// Only HTTPS is accepted, except for loopback addresses used in tests.
pub fn normalize(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidLaunchpad("empty".into()));
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = Url::parse(&with_scheme).map_err(|e| Error::InvalidLaunchpad(e.to_string()))?;
    let host = url
        .host()
        .ok_or_else(|| Error::InvalidLaunchpad("no host".into()))?;
    let loopback = match &host {
        Host::Domain(d) => *d == "localhost",
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    };
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        other => {
            return Err(Error::InvalidLaunchpad(format!(
                "scheme {other} is not allowed"
            )))
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::InvalidLaunchpad("credentials in the address".into()));
    }
    Ok(url.origin().ascii_serialization())
}

/// The panel's name for an Environment whose Launchpad sent no name: the
/// host of the Lazurio address, without scheme or port.
pub fn label(origin: &str) -> String {
    Url::parse(origin)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| origin.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_what_people_paste() {
        assert_eq!(
            normalize(" launchpad.acme.example.com ").unwrap(),
            "https://launchpad.acme.example.com"
        );
        assert_eq!(
            normalize("https://launchpad.acme.example.com/settings/tools?x=1#y").unwrap(),
            "https://launchpad.acme.example.com"
        );
        assert_eq!(
            normalize("http://127.0.0.1:8123/").unwrap(),
            "http://127.0.0.1:8123"
        );
    }

    #[test]
    fn refuses_plain_http_and_credentials() {
        assert!(normalize("http://launchpad.acme.example.com").is_err());
        assert!(normalize("https://user:pw@launchpad.acme.example.com").is_err());
        assert!(normalize("ftp://launchpad.acme.example.com").is_err());
        assert!(normalize("").is_err());
    }

    #[test]
    fn labels() {
        assert_eq!(
            label("https://launchpad.acme.example.com"),
            "launchpad.acme.example.com"
        );
        assert_eq!(label("http://127.0.0.1:8123"), "127.0.0.1");
    }
}
