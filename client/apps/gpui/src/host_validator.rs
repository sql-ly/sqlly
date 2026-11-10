//! Host/instance validation — port of `HostValidator.swift`.
//!
//! Pure, UI-free. Validates SQL Server host strings that may carry an optional
//! instance suffix (`host\instance`) and supports wildcards (`*.domain`), IPv4,
//! IPv6 (with zone ids), and DNS hostnames.

/// Outcome of validating a raw host string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostValidationResult {
    /// The host (and optional instance) is valid.
    Valid,
    /// The trimmed input was empty.
    Empty,
    /// The host portion is not a valid host.
    InvalidHost,
    /// The instance portion (after `\`) is not a valid instance name.
    InvalidInstance,
}

/// Splits `host\instance` (or `host/instance`) into `(host, instance)`.
/// Returns `(host, None)` when no separator is present.
pub fn split_instance(input: &str) -> (&str, Option<&str>) {
    if let Some(idx) = input.find(['\\', '/']) {
        let (h, rest) = input.split_at(idx);
        let inst = &rest[1..];
        if inst.is_empty() {
            (h, None)
        } else {
            (h, Some(inst))
        }
    } else {
        (input, None)
    }
}

/// Local-alias names treated as acceptable even when not DNS-valid.
pub const LOCAL_ALIASES: &[&str] = &["localhost", "local", "(local)"];

/// Validates a raw host string that may include an optional `\instance`
/// suffix. Empty input → `Empty`; bad host → `InvalidHost`; bad instance →
/// `InvalidInstance`; otherwise `Valid`.
pub fn validate_host_with_optional_instance(input: &str) -> HostValidationResult {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return HostValidationResult::Empty;
    }
    let (host, instance) = split_instance(trimmed);
    if !is_acceptable_host(host) {
        return HostValidationResult::InvalidHost;
    }
    if let Some(inst) = instance
        && !is_valid_instance_name(inst)
    {
        return HostValidationResult::InvalidInstance;
    }
    HostValidationResult::Valid
}

/// Convenience: validates a bare host (no instance).
pub fn validate_host(input: &str) -> HostValidationResult {
    validate_host_with_optional_instance(input)
}

/// True when `host` is either a valid host form or a recognized local alias.
pub fn is_acceptable_host(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let lower = host.to_lowercase();
    if LOCAL_ALIASES.contains(&lower.as_str()) {
        return true;
    }
    if is_wildcard_domain(host) {
        return true;
    }
    if is_valid_ipv4(host) {
        return true;
    }
    if is_valid_ipv6(host) {
        return true;
    }
    if is_potentially_valid_hostname(host) {
        return true;
    }
    false
}

/// Wildcards are only allowed as a leading `*.` prefix (e.g. `*.example.com`).
pub fn is_wildcard_domain(host: &str) -> bool {
    if !host.starts_with("*.") {
        return false;
    }
    let rest = &host[2..];
    if rest.is_empty() {
        return false;
    }
    is_potentially_valid_hostname(rest)
}

/// Strict IPv4: four octets 0–255, no leading zeros (except `0` itself).
pub fn is_valid_ipv4(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    for p in parts {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        // No leading zeros (allow single "0").
        if p.len() > 1 && p.starts_with('0') {
            return false;
        }
        match p.parse::<u32>() {
            Ok(n) if n <= 255 => {}
            _ => return false,
        }
    }
    true
}

/// IPv6 with optional zone id (`fe80::1%eth0`). Allows `::` compression once.
pub fn is_valid_ipv6(value: &str) -> bool {
    let addr = match value.split_once('%') {
        Some((a, _zone)) if !a.is_empty() => a,
        _ => value,
    };
    if addr.is_empty() {
        return false;
    }
    // Must contain at least one colon to be IPv6.
    if !addr.contains(':') {
        return false;
    }
    // Reject embedded IPv4 unless preceded by colon.
    let groups: Vec<&str> = addr.split(':').collect();
    // `::` produces empty segments; count them.
    let empty_count = groups.iter().filter(|g| g.is_empty()).count();
    // A bare `::` yields three segments (all empty); leading/trailing `::` also
    // produce empties. More than 3 empty segments is malformed.
    if empty_count > 3 {
        return false;
    }
    // Without `::` (no empties), exactly 8 groups required.
    if empty_count == 0 && groups.len() != 8 {
        return false;
    }
    // With empties (i.e. a `::` is present), the empty segments must be
    // contiguous (form a single `::`). Find the run of empties and confirm it
    // spans all empties.
    if empty_count > 0 {
        let empty_indices: Vec<usize> = groups
            .iter()
            .enumerate()
            .filter(|(_, g)| g.is_empty())
            .map(|(i, _)| i)
            .collect();
        let span = empty_indices.last().unwrap() - empty_indices[0] + 1;
        if span != empty_indices.len() {
            return false;
        }
    }
    // Each non-empty group: 1-4 hex digits, or a trailing IPv4 group.
    for g in &groups {
        if g.is_empty() {
            continue;
        }
        if g.len() > 4 || !g.chars().all(|c| c.is_ascii_hexdigit()) {
            // Could be an embedded IPv4 in the last group.
            if is_valid_ipv4(g) {
                continue;
            }
            return false;
        }
    }
    true
}

/// DNS hostname: labels separated by dots, each 1-63 chars, alphanumeric + hyphen
/// (not leading/trailing). TLD must not be all-numeric. Allows a single dot
/// (`.`) as the root label.
pub fn is_potentially_valid_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if host == "." {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.is_empty() {
        return false;
    }
    for (i, label) in labels.iter().enumerate() {
        if label.is_empty() {
            // Empty intermediate label only allowed via `::`-style for IPv6, not here.
            // A trailing dot (root) is allowed.
            if i == labels.len() - 1 {
                continue;
            }
            return false;
        }
        if label.len() > 63 {
            return false;
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return false;
        }
        if label.starts_with('-') || label.ends_with('-') {
            return false;
        }
    }
    // TLD (last non-empty label) must not be all-numeric.
    let tld = labels.iter().rev().find(|l| !l.is_empty());
    if let Some(t) = tld
        && t.chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    true
}

/// Instance names: alphanumeric, hyphens, underscores; non-empty.
pub fn is_valid_instance_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_rejected() {
        assert_eq!(validate_host(""), HostValidationResult::Empty);
        assert_eq!(validate_host("   "), HostValidationResult::Empty);
    }

    #[test]
    fn local_aliases_accepted() {
        assert_eq!(validate_host("localhost"), HostValidationResult::Valid);
        assert_eq!(validate_host("local"), HostValidationResult::Valid);
        assert_eq!(validate_host("(local)"), HostValidationResult::Valid);
        assert_eq!(validate_host("LOCALHOST"), HostValidationResult::Valid);
    }

    #[test]
    fn valid_ipv4() {
        assert!(is_valid_ipv4("127.0.0.1"));
        assert!(is_valid_ipv4("0.0.0.0"));
        assert!(is_valid_ipv4("255.255.255.255"));
        assert!(is_valid_ipv4("192.168.1.1"));
    }

    #[test]
    fn invalid_ipv4() {
        assert!(!is_valid_ipv4("256.1.1.1"));
        assert!(!is_valid_ipv4("1.2.3"));
        assert!(!is_valid_ipv4("01.2.3.4"));
        assert!(!is_valid_ipv4("1.2.3.4.5"));
        assert!(!is_valid_ipv4("a.b.c.d"));
        assert!(!is_valid_ipv4(""));
    }

    #[test]
    fn valid_ipv6() {
        assert!(is_valid_ipv6("::1"));
        assert!(is_valid_ipv6("::"));
        assert!(is_valid_ipv6("fe80::1"));
        assert!(is_valid_ipv6("2001:db8::1"));
        assert!(is_valid_ipv6("fe80::1%eth0"));
        assert!(is_valid_ipv6("2001:0db8:0000:0000:0000:0000:0000:0001"));
    }

    #[test]
    fn invalid_ipv6() {
        assert!(!is_valid_ipv6(":::"));
        assert!(!is_valid_ipv6("gggg::1"));
        assert!(!is_valid_ipv6("127.0.0.1"));
        assert!(!is_valid_ipv6(""));
        assert!(!is_valid_ipv6("1:2:3:4:5:6:7:8:9"));
    }

    #[test]
    fn valid_hostname() {
        assert!(is_potentially_valid_hostname("example.com"));
        assert!(is_potentially_valid_hostname("sub.example.com"));
        assert!(is_potentially_valid_hostname("host-name.example.org"));
        assert!(is_potentially_valid_hostname("."));
        assert!(is_potentially_valid_hostname("host."));
    }

    #[test]
    fn invalid_hostname() {
        assert!(!is_potentially_valid_hostname(""));
        assert!(!is_potentially_valid_hostname("-bad.example.com"));
        assert!(!is_potentially_valid_hostname("bad-.example.com"));
        assert!(!is_potentially_valid_hostname("example.123"));
        assert!(!is_potentially_valid_hostname("exa mple.com"));
    }

    #[test]
    fn wildcard_domain() {
        assert!(is_wildcard_domain("*.example.com"));
        assert!(!is_wildcard_domain("example.*"));
        assert!(!is_wildcard_domain("*"));
        assert!(!is_wildcard_domain("*.*.com"));
    }

    #[test]
    fn instance_split() {
        assert_eq!(split_instance("host"), ("host", None));
        assert_eq!(split_instance("host\\instance"), ("host", Some("instance")));
        assert_eq!(split_instance("host/inst"), ("host", Some("inst")));
        assert_eq!(split_instance("host\\"), ("host", None));
    }

    #[test]
    fn validate_with_instance() {
        assert_eq!(
            validate_host("localhost\\SQLEXPRESS"),
            HostValidationResult::Valid
        );
        assert_eq!(
            validate_host("localhost\\bad instance!"),
            HostValidationResult::InvalidInstance
        );
        assert_eq!(
            validate_host("bad host!\\instance"),
            HostValidationResult::InvalidHost
        );
    }

    #[test]
    fn instance_name_rules() {
        assert!(is_valid_instance_name("SQLEXPRESS"));
        assert!(is_valid_instance_name("SQL_Express-1"));
        assert!(!is_valid_instance_name(""));
        assert!(!is_valid_instance_name("bad instance"));
        assert!(!is_valid_instance_name("bad!"));
    }
}
