//! Turns user input into a flat list of ping targets.
//!
//! Accepted forms (any mix, separated by commas, semicolons or whitespace):
//!   * DNS name:            `example.com`
//!   * Single IPv4 / IPv6:  `8.8.8.8`, `2001:4860:4860::8888`
//!   * CIDR subnet:         `192.168.1.0/24`   (network & broadcast excluded for /30 and shorter)
//!   * Range:               `10.1.1.1-255` or `10.1.1.1-10.1.2.20`
//!   * CSV file contents:   any of the above, one or more per line

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Maximum number of targets a single add / import may create.
pub const MAX_TARGETS: usize = 4096;

#[derive(Default, Debug)]
pub struct Expanded {
    pub targets: Vec<String>,
    pub errors: Vec<String>,
}

/// Expand text typed into the target box.
pub fn expand_input(text: &str) -> Expanded {
    collect(
        text.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
            .filter(|s| !s.is_empty()),
    )
}

/// Expand the contents of a CSV / text file. Blank lines and `#` comments are
/// ignored, and a header row such as `host,ip` is skipped.
pub fn expand_csv(text: &str) -> Expanded {
    const HEADERS: [&str; 8] = [
        "host", "hostname", "address", "ip", "name", "target", "dns", "ipaddress",
    ];
    let mut cells: Vec<&str> = Vec::new();
    let mut first_row = true;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let row: Vec<&str> = line
            .split(|c| c == ',' || c == ';' || c == '\t')
            .map(|c| c.trim().trim_matches('"').trim())
            .filter(|c| !c.is_empty())
            .collect();
        if row.is_empty() {
            continue;
        }
        if first_row {
            first_row = false;
            if row
                .iter()
                .all(|c| HEADERS.contains(&c.to_ascii_lowercase().as_str()))
            {
                continue;
            }
        }
        cells.extend(row);
    }
    collect(cells.into_iter())
}

fn collect<'a>(tokens: impl Iterator<Item = &'a str>) -> Expanded {
    let mut out = Expanded::default();
    let mut seen = HashSet::new();
    for tok in tokens {
        match expand_token(tok) {
            Ok(list) => {
                for t in list {
                    if seen.insert(t.to_ascii_lowercase()) {
                        out.targets.push(t);
                    }
                }
            }
            Err(e) => out.errors.push(e),
        }
    }
    if out.targets.len() > MAX_TARGETS {
        out.errors.push(format!(
            "{} targets requested; the limit per add is {MAX_TARGETS}",
            out.targets.len()
        ));
        out.targets.clear();
    }
    out
}

fn expand_token(tok: &str) -> Result<Vec<String>, String> {
    let tok = tok.trim();
    let tok = tok
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .unwrap_or(tok);
    if tok.is_empty() {
        return Ok(vec![]);
    }

    // CIDR
    if let Some((addr, len)) = tok.split_once('/') {
        let ip: IpAddr = addr
            .trim()
            .parse()
            .map_err(|_| format!("'{tok}': not a valid network address"))?;
        let len: u32 = len
            .trim()
            .parse()
            .map_err(|_| format!("'{tok}': invalid prefix length"))?;
        return expand_cidr(ip, len).map_err(|e| format!("'{tok}': {e}"));
    }

    // Single IP
    if let Ok(ip) = tok.parse::<IpAddr>() {
        return Ok(vec![ip.to_string()]);
    }

    // IPv4 range: a.b.c.d-N  or  a.b.c.d-w.x.y.z
    if let Some((l, r)) = tok.split_once('-') {
        if let Ok(start) = l.trim().parse::<Ipv4Addr>() {
            return expand_range(start, r).map_err(|e| format!("'{tok}': {e}"));
        }
    }

    // Looks like an IPv4 address but did not parse (e.g. 10.1.1.300)
    if tok.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(format!("'{tok}': invalid IPv4 address"));
    }

    if valid_hostname(tok) {
        Ok(vec![tok.to_string()])
    } else {
        Err(format!("'{tok}': not a valid hostname or address"))
    }
}

fn valid_hostname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && !s.starts_with(|c| c == '-' || c == '.')
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '.' || c == '_')
}

fn expand_range(start: Ipv4Addr, end_text: &str) -> Result<Vec<String>, String> {
    let s = u32::from(start);
    let end_text = end_text.trim();
    let e = if let Ok(full) = end_text.parse::<Ipv4Addr>() {
        u32::from(full)
    } else if let Ok(last) = end_text.parse::<u8>() {
        (s & 0xFFFF_FF00) | last as u32
    } else {
        return Err(
            "range end must be a last octet (10.1.1.1-254) or a full IPv4 address".to_string(),
        );
    };
    if e < s {
        return Err("range end is before the range start".to_string());
    }
    let count = (e - s) as u64 + 1;
    if count > MAX_TARGETS as u64 {
        return Err(format!(
            "range has {count} addresses; the limit is {MAX_TARGETS}"
        ));
    }
    Ok((s..=e).map(|n| Ipv4Addr::from(n).to_string()).collect())
}

fn expand_cidr(ip: IpAddr, len: u32) -> Result<Vec<String>, String> {
    match ip {
        IpAddr::V4(v4) => {
            if len > 32 {
                return Err("IPv4 prefix length must be between 0 and 32".to_string());
            }
            let host_bits = 32 - len;
            let count = 1u64 << host_bits;
            // /31 and /32 have no network/broadcast addresses to exclude.
            let usable = if len >= 31 { count } else { count - 2 };
            if usable > MAX_TARGETS as u64 {
                return Err(format!(
                    "subnet has {usable} hosts; the limit is {MAX_TARGETS} (use /20 or smaller)"
                ));
            }
            let mask = if host_bits == 0 { u32::MAX } else { u32::MAX << host_bits };
            let base = u32::from(v4) & mask;
            let (first, last) = if len >= 31 {
                (base, base + (count as u32 - 1))
            } else {
                (base + 1, base + (count as u32 - 2))
            };
            Ok((first..=last)
                .map(|n| Ipv4Addr::from(n).to_string())
                .collect())
        }
        IpAddr::V6(v6) => {
            if len > 128 {
                return Err("IPv6 prefix length must be between 0 and 128".to_string());
            }
            let host_bits = 128 - len;
            if host_bits > 12 {
                return Err(format!(
                    "subnet is too large; the limit is {MAX_TARGETS} addresses (use /116 or longer)"
                ));
            }
            let mask = if host_bits == 0 { u128::MAX } else { u128::MAX << host_bits };
            let base = u128::from(v6) & mask;
            let count = 1u128 << host_bits;
            Ok((0..count)
                .map(|i| Ipv6Addr::from(base + i).to_string())
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Vec<String> {
        let e = expand_input(s);
        assert!(e.errors.is_empty(), "unexpected errors: {:?}", e.errors);
        e.targets
    }

    #[test]
    fn dns_and_single_ips() {
        assert_eq!(ok("example.com"), vec!["example.com"]);
        assert_eq!(ok("8.8.8.8"), vec!["8.8.8.8"]);
        assert_eq!(ok("2001:4860:4860:0:0:0:0:8888"), vec!["2001:4860:4860::8888"]);
        assert_eq!(ok("[::1]"), vec!["::1"]);
        assert_eq!(ok("my-host_1.lan"), vec!["my-host_1.lan"]);
    }

    #[test]
    fn cidr_v4() {
        let t = ok("192.168.1.0/24");
        assert_eq!(t.len(), 254);
        assert_eq!(t.first().unwrap(), "192.168.1.1");
        assert_eq!(t.last().unwrap(), "192.168.1.254");
        assert_eq!(ok("192.168.1.77/30"), vec!["192.168.1.77", "192.168.1.78"]);
        assert_eq!(ok("10.0.0.4/31"), vec!["10.0.0.4", "10.0.0.5"]);
        assert_eq!(ok("10.0.0.9/32"), vec!["10.0.0.9"]);
        assert_eq!(ok("10.0.0.0/20").len(), 4094);
        assert!(!expand_input("10.0.0.0/19").errors.is_empty());
        assert!(!expand_input("10.0.0.0/8").errors.is_empty());
        assert!(!expand_input("10.0.0.0/33").errors.is_empty());
    }

    #[test]
    fn cidr_v6() {
        assert_eq!(ok("2001:db8::/126").len(), 4);
        assert_eq!(ok("2001:db8::5/128"), vec!["2001:db8::5"]);
        assert!(!expand_input("2001:db8::/64").errors.is_empty());
    }

    #[test]
    fn ranges() {
        let t = ok("10.1.1.1-255");
        assert_eq!(t.len(), 255);
        assert_eq!(t.last().unwrap(), "10.1.1.255");
        assert_eq!(ok("10.1.1.250-10.1.2.2").len(), 9);
        assert!(!expand_input("10.1.1.9-3").errors.is_empty());
        assert!(!expand_input("10.1.1.1-999").errors.is_empty());
        assert!(!expand_input("10.0.0.0-10.1.0.0").errors.is_empty());
    }

    #[test]
    fn lists_and_dedupe() {
        let t = ok("8.8.8.8, example.com 8.8.8.8;10.0.0.1-3,EXAMPLE.com");
        assert_eq!(
            t,
            vec!["8.8.8.8", "example.com", "10.0.0.1", "10.0.0.2", "10.0.0.3"]
        );
    }

    #[test]
    fn bad_input_reports_errors_but_keeps_good() {
        // Whitespace separates tokens, so "bad host!" is the tokens "bad" and "host!".
        let e = expand_input("good.com, 10.1.1.300, bad host!, ok2.com");
        assert_eq!(e.targets, vec!["good.com", "bad", "ok2.com"]);
        assert_eq!(e.errors.len(), 2);
    }

    #[test]
    fn csv() {
        let text = "\u{feff}Host,IP\n# comment\n\"example.com\",8.8.8.8\n10.0.0.0/30\n\nfoo.lan; bar.lan\n";
        let e = expand_csv(text);
        assert!(e.errors.is_empty(), "{:?}", e.errors);
        assert_eq!(
            e.targets,
            vec!["example.com", "8.8.8.8", "10.0.0.1", "10.0.0.2", "foo.lan", "bar.lan"]
        );
    }

    #[test]
    fn too_many_targets_rejected() {
        let e = expand_input("10.0.0.0/20, 10.1.0.0/20");
        assert!(e.targets.is_empty());
        assert!(!e.errors.is_empty());
    }
}