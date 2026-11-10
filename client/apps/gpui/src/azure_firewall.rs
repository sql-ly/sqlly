//! Azure SQL firewall error detection and ARM server matching.
//!
//! Port of the pure parts of `AzureFirewallService.swift`. Detects the
//! firewall-block error (SQL error code 40615), extracts the blocked client
//! IP, builds a firewall rule name, and matches a target server FQDN against
//! an ARM server list.

use serde::Deserialize;

#[allow(dead_code)]
/// SQL error code returned when a client IP is blocked by the server firewall.
pub const FIREWALL_BLOCKED_CODE: u32 = 40615;

/// Pure helpers for recognizing the Azure SQL firewall-block error.
pub struct AzureSqlFirewallError;

impl AzureSqlFirewallError {
    #[allow(dead_code)]
    /// Whether `message` indicates a firewall block. Matches the Swift rules:
    /// `(code: 40615)` or `code: 40615`, or both `40615` and
    /// `not allowed to access`, or both `client with ip address` and
    /// `not allowed to access`.
    pub fn is_firewall_block(message: &str) -> bool {
        let lower = message.to_lowercase();
        if lower.contains("(code: 40615)") || lower.contains("code: 40615") {
            return true;
        }
        if lower.contains("40615") && lower.contains("not allowed to access") {
            return true;
        }
        lower.contains("client with ip address") && lower.contains("not allowed to access")
    }

    #[allow(dead_code)]
    /// Extract the blocked client IPv4 from a firewall-block error message.
    /// Prefers the `IP address 'X'` form; falls back to the first valid IPv4
    /// anywhere in the message.
    pub fn extract_blocked_ip(message: &str) -> Option<String> {
        if let Some(ip) = extract_quoted_ip(message)
            && is_valid_ipv4(&ip)
        {
            return Some(ip);
        }
        first_ipv4(message).filter(|v| is_valid_ipv4(v))
    }

    /// Validate that `value` is a well-formed IPv4 address.
    pub fn is_valid_ipv4(value: &str) -> bool {
        is_valid_ipv4(value)
    }
}

#[allow(dead_code)]
fn extract_quoted_ip(message: &str) -> Option<String> {
    let lower = message.to_lowercase();
    let key = "ip address '";
    let idx = lower.find(key)?;
    let rest = message[idx + key.len()..].trim_start();
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

#[allow(dead_code)]
/// The first valid-looking IPv4 substring in `text`: split on any non-digit /
/// non-dot boundary and return the first all-digit/dot run.
pub fn first_ipv4(text: &str) -> Option<String> {
    let mut current = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() || c == '.' {
            current.push(c);
        } else if !current.is_empty() {
            if is_valid_ipv4(&current) {
                return Some(current);
            }
            current.clear();
        }
    }
    if is_valid_ipv4(&current) {
        Some(current)
    } else {
        None
    }
}

/// Validate `value` as 4 dot-separated octets in the range 0-255.
pub fn is_valid_ipv4(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    for p in parts {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        match p.parse::<u32>() {
            Ok(n) if n <= 255 => {}
            _ => return false,
        }
    }
    true
}

/// Longest rule name ARM accepts for `Microsoft.Sql/servers/firewallRules`.
pub const MAX_RULE_NAME_LEN: usize = 128;

/// Replace every character ARM rejects in a firewall rule name with `-`.
/// Legal set is `[a-zA-Z0-9_-]`; everything else (including `@`, `.`, and
/// spaces) folds to a hyphen so a name typed in the dialog is always
/// submittable rather than bounced by ARM with a validation error.
pub fn sanitize_rule_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Build a firewall rule name from a username and a date (yyyymmdd).
/// Format: `SQLLY-{local_part}-{yyyymmdd}`, sanitized to `[a-zA-Z0-9_-]`.
pub fn firewall_rule_name(username: &str, yyyymmdd: &str) -> String {
    let local = username.split('@').next().unwrap_or(username);
    format!("SQLLY-{}-{}", sanitize_rule_name(local), yyyymmdd)
}

/// Numeric value of a dotted-quad IPv4, for ordering a start/end pair.
/// `None` when `value` is not a well-formed IPv4.
pub fn ipv4_to_u32(value: &str) -> Option<u32> {
    if !is_valid_ipv4(value) {
        return None;
    }
    let mut out: u32 = 0;
    for part in value.split('.') {
        out = (out << 8) | part.parse::<u32>().ok()?;
    }
    Some(out)
}

/// Validate what the "add firewall rule" dialog is about to submit.
///
/// Returns the reason the request cannot be sent, or `None` when it is
/// well-formed. Checked here rather than at the ARM boundary so the user is
/// told what is wrong *before* a round trip that would come back as an opaque
/// `InvalidResourceName` / `InvalidIpAddress` from Azure.
pub fn validate_firewall_request(rule_name: &str, start_ip: &str, end_ip: &str) -> Option<String> {
    let name = rule_name.trim();
    if name.is_empty() {
        return Some(crate::localization::text(
            "connections.firewall.validation_name_required",
        ));
    }
    if name.chars().count() > MAX_RULE_NAME_LEN {
        return Some(crate::localization::text(
            "connections.firewall.validation_name_too_long",
        ));
    }
    if sanitize_rule_name(name) != name {
        return Some(crate::localization::text(
            "connections.firewall.validation_name_characters",
        ));
    }
    let (Some(start), Some(end)) = (ipv4_to_u32(start_ip.trim()), ipv4_to_u32(end_ip.trim()))
    else {
        return Some(crate::localization::text(
            "connections.firewall.validation_ipv4_required",
        ));
    };
    if start > end {
        return Some(crate::localization::text(
            "connections.firewall.validation_range_order",
        ));
    }
    None
}

#[allow(dead_code)]
/// One server discovered via ARM, matched against a target FQDN.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ArmServerMatch {
    pub id: String,
    pub name: String,
    /// The FQDN, pulled from the nested `properties.fullyQualifiedDomainName`
    /// field of the ARM response. Absent if not provided.
    #[serde(default)]
    pub fqdn: Option<String>,
}

#[allow(dead_code)]
/// Internal mirror of the ARM JSON `properties` object for deserialization.
#[derive(Deserialize, Default)]
struct ArmServerProperties {
    #[serde(default, rename = "fullyQualifiedDomainName")]
    fqdn: Option<String>,
}

#[allow(dead_code)]
/// Internal mirror of the full ARM server object for deserialization.
#[derive(Deserialize)]
struct ArmServerEntry {
    id: String,
    name: String,
    #[serde(default)]
    properties: ArmServerProperties,
}

impl ArmServerEntry {
    #[allow(dead_code)]
    fn into_match(self) -> ArmServerMatch {
        ArmServerMatch {
            id: self.id,
            name: self.name,
            fqdn: self.properties.fqdn,
        }
    }
}

#[allow(dead_code)]
/// Helpers for locating a server in an ARM list by FQDN or short name.
pub struct ArmServerLocator;

impl ArmServerLocator {
    #[allow(dead_code)]
    /// The short name of a FQDN: the first label before the first `.`,
    /// lowercased. Returns the whole input (lowercased) if there is no `.`.
    pub fn short_name(fqdn: &str) -> String {
        let first = fqdn.split('.').next().unwrap_or(fqdn);
        first.to_lowercase()
    }

    #[allow(dead_code)]
    /// Parse the ARM server list JSON (`{"value":[...]}`), flattening the
    /// nested `properties.fullyQualifiedDomainName` into `fqdn`.
    pub fn parse_servers(json: &str) -> Option<Vec<ArmServerMatch>> {
        #[derive(Deserialize)]
        struct Resp {
            value: Vec<ArmServerEntry>,
        }
        serde_json::from_str::<Resp>(json)
            .ok()
            .map(|r| r.value.into_iter().map(|e| e.into_match()).collect())
    }

    #[allow(dead_code)]
    /// Find the server whose FQDN matches `target_fqdn` exactly, or whose
    /// short name matches the target's short name. Returns the first match.
    pub fn find_match<'a>(
        target_fqdn: &str,
        servers: &'a [ArmServerMatch],
    ) -> Option<&'a ArmServerMatch> {
        let target_lower = target_fqdn.to_lowercase();
        if let Some(m) = servers.iter().find(|s| {
            s.fqdn
                .as_deref()
                .map(|f| f.eq_ignore_ascii_case(&target_lower))
                .unwrap_or(false)
        }) {
            return Some(m);
        }
        let short = Self::short_name(target_fqdn);
        servers.iter().find(|s| {
            s.fqdn
                .as_deref()
                .map(|f| Self::short_name(f) == short)
                .unwrap_or_else(|| s.name.to_lowercase() == short)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_code_paren_form() {
        assert!(AzureSqlFirewallError::is_firewall_block(
            "Error: (code: 40615) client with ip address not allowed"
        ));
    }

    #[test]
    fn detects_code_plain_form() {
        assert!(AzureSqlFirewallError::is_firewall_block("code: 40615"));
    }

    #[test]
    fn detects_40615_and_not_allowed() {
        assert!(AzureSqlFirewallError::is_firewall_block(
            "Error 40615: not allowed to access the server"
        ));
    }

    #[test]
    fn detects_client_ip_and_not_allowed() {
        assert!(AzureSqlFirewallError::is_firewall_block(
            "client with ip address '1.2.3.4' is not allowed to access the server"
        ));
    }

    #[test]
    fn non_firewall_message_not_detected() {
        assert!(!AzureSqlFirewallError::is_firewall_block(
            "login failed for user 'sa'"
        ));
    }

    #[test]
    fn extract_blocked_ip_from_quoted_form() {
        let ip = AzureSqlFirewallError::extract_blocked_ip(
            "Client with IP address '203.0.113.5' is not allowed to access the server.",
        );
        assert_eq!(ip.as_deref(), Some("203.0.113.5"));
    }

    #[test]
    fn extract_blocked_ip_falls_back_to_first_ipv4() {
        let ip =
            AzureSqlFirewallError::extract_blocked_ip("connection from 198.51.100.7 was blocked");
        assert_eq!(ip.as_deref(), Some("198.51.100.7"));
    }

    #[test]
    fn extract_blocked_ip_returns_none_when_no_ip() {
        assert!(AzureSqlFirewallError::extract_blocked_ip("login failed").is_none());
    }

    #[test]
    fn extract_blocked_ip_ignores_invalid_quoted_ip() {
        let ip = AzureSqlFirewallError::extract_blocked_ip("IP address 'not.an.ip' blocked");
        assert!(ip.is_none());
    }

    #[test]
    fn validates_good_ipv4() {
        assert!(is_valid_ipv4("0.0.0.0"));
        assert!(is_valid_ipv4("255.255.255.255"));
        assert!(is_valid_ipv4("10.0.0.1"));
    }

    #[test]
    fn rejects_bad_ipv4() {
        assert!(!is_valid_ipv4("256.0.0.1"));
        assert!(!is_valid_ipv4("1.2.3"));
        assert!(!is_valid_ipv4("1.2.3.4.5"));
        assert!(!is_valid_ipv4("a.b.c.d"));
        assert!(!is_valid_ipv4(""));
        assert!(!is_valid_ipv4("1.2.3."));
    }

    #[test]
    fn first_ipv4_finds_embedded_address() {
        assert_eq!(first_ipv4("foo 10.0.0.5 bar").as_deref(), Some("10.0.0.5"));
    }

    #[test]
    fn first_ipv4_skips_non_ip_numbers() {
        assert_eq!(first_ipv4("error 40615 server").as_deref(), None);
    }

    #[test]
    fn firewall_rule_name_basic() {
        assert_eq!(
            firewall_rule_name("alice", "20260101"),
            "SQLLY-alice-20260101"
        );
    }

    #[test]
    fn firewall_rule_name_strips_domain() {
        assert_eq!(
            firewall_rule_name("bob@example.com", "20260101"),
            "SQLLY-bob-20260101"
        );
    }

    #[test]
    fn firewall_rule_name_sanitizes_special_chars() {
        assert_eq!(
            firewall_rule_name("a.b+c", "20260101"),
            "SQLLY-a-b-c-20260101"
        );
    }

    #[test]
    fn sanitize_rule_name_folds_illegal_characters() {
        assert_eq!(sanitize_rule_name("home office"), "home-office");
        assert_eq!(sanitize_rule_name("alex@contoso.com"), "alex-contoso-com");
        assert_eq!(sanitize_rule_name("keep_me-1"), "keep_me-1");
    }

    #[test]
    fn ipv4_to_u32_orders_addresses() {
        assert!(ipv4_to_u32("10.0.0.1") < ipv4_to_u32("10.0.0.2"));
        assert_eq!(ipv4_to_u32("0.0.0.0"), Some(0));
        assert_eq!(ipv4_to_u32("255.255.255.255"), Some(u32::MAX));
        assert_eq!(ipv4_to_u32("not.an.ip.here"), None);
    }

    #[test]
    fn validate_accepts_a_single_address_rule() {
        assert!(
            validate_firewall_request("SQLLY-matt-20260803", "203.0.113.5", "203.0.113.5")
                .is_none()
        );
    }

    #[test]
    fn validate_accepts_a_range() {
        assert!(validate_firewall_request("office", "203.0.113.1", "203.0.113.254").is_none());
    }

    #[test]
    fn validate_rejects_empty_rule_name() {
        assert!(
            validate_firewall_request("   ", "203.0.113.5", "203.0.113.5")
                .unwrap()
                .contains("Enter a name")
        );
    }

    #[test]
    fn validate_rejects_illegal_rule_name_characters() {
        assert!(
            validate_firewall_request("my rule", "203.0.113.5", "203.0.113.5")
                .unwrap()
                .contains("letters, digits")
        );
    }

    #[test]
    fn validate_rejects_overlong_rule_name() {
        let long = "a".repeat(MAX_RULE_NAME_LEN + 1);
        assert!(
            validate_firewall_request(&long, "203.0.113.5", "203.0.113.5")
                .unwrap()
                .contains("128 characters")
        );
    }

    #[test]
    fn validate_rejects_malformed_ip() {
        assert!(validate_firewall_request("rule", "203.0.113", "203.0.113.5").is_some());
    }

    #[test]
    fn validate_rejects_inverted_range() {
        assert!(
            validate_firewall_request("rule", "203.0.113.9", "203.0.113.1")
                .unwrap()
                .contains("start address")
        );
    }

    #[test]
    fn short_name_takes_first_label_lowercased() {
        assert_eq!(
            ArmServerLocator::short_name("Server.prod.azure.com"),
            "server"
        );
    }

    #[test]
    fn short_name_without_dot_returns_whole_lowercased() {
        assert_eq!(ArmServerLocator::short_name("MyServer"), "myserver");
    }

    #[test]
    fn parse_servers_extracts_value_array() {
        let json = r#"{"value":[{"id":"a","name":"srv","properties":{"fullyQualifiedDomainName":"srv.azure.com"}}]}"#;
        let servers = ArmServerLocator::parse_servers(json).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "srv");
        assert_eq!(servers[0].fqdn.as_deref(), Some("srv.azure.com"));
    }

    #[test]
    fn parse_servers_returns_none_on_bad_json() {
        assert!(ArmServerLocator::parse_servers("not json").is_none());
    }

    #[test]
    fn find_match_exact_fqdn_case_insensitive() {
        let servers = vec![ArmServerMatch {
            id: "a".into(),
            name: "srv".into(),
            fqdn: Some("Srv.Azure.com".into()),
        }];
        let m = ArmServerLocator::find_match("srv.azure.com", &servers);
        assert!(m.is_some());
        assert_eq!(m.unwrap().id, "a");
    }

    #[test]
    fn find_match_short_name_fallback() {
        let servers = vec![ArmServerMatch {
            id: "a".into(),
            name: "MyServer".into(),
            fqdn: Some("MyServer.prod.azure.com".into()),
        }];
        let m = ArmServerLocator::find_match("myserver", &servers);
        assert!(m.is_some());
    }

    #[test]
    fn find_match_returns_none_when_not_found() {
        let servers = vec![ArmServerMatch {
            id: "a".into(),
            name: "srv".into(),
            fqdn: Some("srv.azure.com".into()),
        }];
        assert!(ArmServerLocator::find_match("other", &servers).is_none());
    }
}
