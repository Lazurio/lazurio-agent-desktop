//! Which channel URL the computer accepts (contract v1, section 2).
//!
//! Everywhere the channel is `wss://` on the Launchpad's own host, with
//! ordinary certificate checks. Plain `ws://` is accepted only on a loopback
//! address (`127.0.0.0/8`, `::1`, `localhost`) and only when the Launchpad
//! itself is `http://` on that same loopback host: a tunnel to an
//! Environment, or a test server. Nothing else ever goes without TLS.

use url::{Host, Url};

pub const ATTACH_PATH: &str = "/.lazurio/computers/attach";

/// Whether the URL's host is a loopback address or `localhost`.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Whether the Launchpad is reached over plain HTTP on loopback (a tunnel).
fn plain_loopback_launchpad(launchpad: &Url) -> bool {
    launchpad.scheme() == "http" && is_loopback(launchpad)
}

/// The channel URL for an Environment whose pairing result had none (v0):
/// the Launchpad's host with the attach path.
pub fn derive_channel_url(launchpad_origin: &str) -> Option<String> {
    let mut url = Url::parse(launchpad_origin).ok()?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" if is_loopback(&url) => "ws",
        _ => return None,
    };
    url.set_scheme(scheme).ok()?;
    url.set_path(ATTACH_PATH);
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

/// Checks the `channel.url` of a pairing result: `wss` (plain `ws` only on
/// loopback behind an `http` loopback Launchpad), the Launchpad's own host,
/// the attach path, and no credentials, query or fragment.
pub fn validate_channel_url(raw: &str, launchpad_origin: &str) -> Result<String, String> {
    let url = Url::parse(raw).map_err(|e| e.to_string())?;
    let launchpad = Url::parse(launchpad_origin).map_err(|e| e.to_string())?;
    match url.scheme() {
        "wss" => {}
        "ws" if is_loopback(&url) && plain_loopback_launchpad(&launchpad) => {}
        "ws" => return Err("plain ws is allowed only through a loopback tunnel".into()),
        other => return Err(format!("scheme {other} is not allowed")),
    }
    if url.host_str() != launchpad.host_str() {
        return Err("the channel is not on the Launchpad's host".into());
    }
    if url.path() != ATTACH_PATH {
        return Err("unexpected channel path".into());
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("unexpected parts in the channel URL".into());
    }
    Ok(url.to_string())
}

/// Whether [`crate::wire::connect`] may use TLS-less `ws` for this URL. The
/// resolved addresses must be loopback too (see `wire::connect`).
pub fn plain_allowed(url: &Url) -> bool {
    url.scheme() == "ws" && is_loopback(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATTACH: &str = "/.lazurio/computers/attach";

    #[test]
    fn derives_the_channel_from_the_launchpad() {
        assert_eq!(
            derive_channel_url("https://launchpad.acme.example.com").unwrap(),
            format!("wss://launchpad.acme.example.com{ATTACH}")
        );
        for (lp, ws) in [
            ("http://127.0.0.1:47943", "ws://127.0.0.1:47943"),
            ("http://[::1]:47943", "ws://[::1]:47943"),
            ("http://localhost:47943", "ws://localhost:47943"),
        ] {
            assert_eq!(derive_channel_url(lp).unwrap(), format!("{ws}{ATTACH}"));
        }
        // Plain HTTP off loopback gets no channel at all.
        assert_eq!(
            derive_channel_url("http://launchpad.acme.example.com"),
            None
        );
        assert_eq!(derive_channel_url("http://192.0.2.10:47943"), None);
    }

    #[test]
    fn plain_ws_only_through_a_loopback_tunnel() {
        for (channel, lp) in [
            ("ws://127.0.0.1:47943", "http://127.0.0.1:47943"),
            ("ws://127.0.0.1:9", "http://127.0.0.1:47943"),
            ("ws://[::1]:47943", "http://[::1]:47943"),
            ("ws://localhost:47943", "http://localhost:47943"),
        ] {
            let raw = format!("{channel}{ATTACH}");
            assert_eq!(validate_channel_url(&raw, lp).unwrap(), raw);
        }
        for (channel, lp) in [
            // An HTTPS Launchpad never hands out plain ws, even on loopback.
            ("ws://127.0.0.1:47943", "https://127.0.0.1:47943"),
            ("ws://localhost:47943", "https://localhost"),
            // Off loopback: never.
            (
                "ws://launchpad.acme.example.com",
                "https://launchpad.acme.example.com",
            ),
            (
                "ws://launchpad.acme.example.com",
                "http://launchpad.acme.example.com",
            ),
            ("ws://192.0.2.10:47943", "http://192.0.2.10:47943"),
            ("ws://[::2]:47943", "http://[::2]:47943"),
            // Loopback, but not the Launchpad's host.
            ("ws://127.0.0.2:47943", "http://127.0.0.1:47943"),
            ("ws://localhost:47943", "http://127.0.0.1:47943"),
        ] {
            let raw = format!("{channel}{ATTACH}");
            assert!(validate_channel_url(&raw, lp).is_err(), "{raw} for {lp}");
        }
    }

    #[test]
    fn wss_everywhere_else_on_the_launchpads_host() {
        let lp = "https://launchpad.acme.example.com";
        let good = format!("wss://launchpad.acme.example.com{ATTACH}");
        assert_eq!(validate_channel_url(&good, lp).unwrap(), good);
        // wss through a tunnel is fine too (TLS is never a downgrade).
        assert!(validate_channel_url(
            &format!("wss://127.0.0.1:47943{ATTACH}"),
            "http://127.0.0.1:47943"
        )
        .is_ok());
        for bad in [
            format!("wss://evil.example.com{ATTACH}"),
            "wss://launchpad.acme.example.com/other".to_string(),
            format!("wss://u:p@launchpad.acme.example.com{ATTACH}"),
            format!("wss://launchpad.acme.example.com{ATTACH}?x=1"),
            format!("wss://launchpad.acme.example.com{ATTACH}#x"),
            format!("https://launchpad.acme.example.com{ATTACH}"),
            format!("http://127.0.0.1:47943{ATTACH}"),
        ] {
            assert!(validate_channel_url(&bad, lp).is_err(), "{bad}");
        }
    }

    #[test]
    fn plain_connect_only_for_loopback_ws() {
        let ok = |s: &str| plain_allowed(&Url::parse(s).unwrap());
        assert!(ok("ws://127.0.0.1:47943/x"));
        assert!(ok("ws://[::1]:47943/x"));
        assert!(ok("ws://LOCALHOST:47943/x"));
        assert!(!ok("ws://192.0.2.10:47943/x"));
        assert!(!ok("ws://launchpad.acme.example.com/x"));
        assert!(!ok("wss://127.0.0.1:47943/x"));
    }
}
