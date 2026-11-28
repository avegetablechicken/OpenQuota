use std::net::{IpAddr, Ipv4Addr};

use ipnet::IpNet;
use reqwest::Url;
use system_configuration::sys::schema_definitions::{
    kSCPropNetProxiesExceptionsList, kSCPropNetProxiesExcludeSimpleHostnames,
};

use super::{flag_setting, string_array_setting, ProxySettings};

#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct BypassRules {
    exclude_simple_hostnames: bool,
    rules: Vec<BypassRule>,
}

#[derive(Clone, PartialEq, Eq)]
enum BypassRule {
    All,
    Ip(IpAddr),
    Network(IpNet),
    Host {
        value: String,
        include_subdomains: bool,
    },
    Pattern(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum HostMatch {
    Exact,
    IncludeSubdomains,
}

impl BypassRules {
    pub(super) fn from_system_settings(settings: &ProxySettings) -> Self {
        let entries = string_array_setting(settings, unsafe { kSCPropNetProxiesExceptionsList });
        let exclude_simple =
            flag_setting(settings, unsafe { kSCPropNetProxiesExcludeSimpleHostnames });
        Self::from_entries(
            entries.iter().map(String::as_str),
            exclude_simple,
            HostMatch::Exact,
        )
    }

    pub(super) fn from_comma_list(value: &str, exclude_simple: bool) -> Self {
        Self::from_entries(
            value.split(','),
            exclude_simple,
            HostMatch::IncludeSubdomains,
        )
    }

    pub(super) fn from_entries<'a>(
        entries: impl IntoIterator<Item = &'a str>,
        exclude_simple: bool,
        host_match: HostMatch,
    ) -> Self {
        let mut result = Self {
            exclude_simple_hostnames: exclude_simple,
            rules: Vec::new(),
        };
        for raw in entries {
            for raw in raw.split(',') {
                let raw = raw.trim().trim_end_matches('.').to_ascii_lowercase();
                if raw.is_empty() {
                    continue;
                }
                if raw == "<local>" {
                    result.exclude_simple_hostnames = true;
                } else if raw == "*" {
                    result.rules.push(BypassRule::All);
                } else if let Some(network) = parse_ip_network(&raw) {
                    result.rules.push(BypassRule::Network(network));
                } else if let Ok(ip) = raw.parse::<IpAddr>() {
                    result.rules.push(BypassRule::Ip(ip));
                } else if raw.contains('*') || raw.contains('?') {
                    result.rules.push(BypassRule::Pattern(raw));
                } else {
                    result.rules.push(BypassRule::Host {
                        value: raw.trim_start_matches('.').to_owned(),
                        include_subdomains: matches!(host_match, HostMatch::IncludeSubdomains),
                    });
                }
            }
        }
        result
    }

    pub(super) fn matches(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if self.exclude_simple_hostnames && !host.contains('.') && host.parse::<IpAddr>().is_err() {
            return true;
        }
        let ip = host.parse::<IpAddr>().ok();
        self.rules.iter().any(|rule| match rule {
            BypassRule::All => true,
            BypassRule::Ip(expected) => ip.as_ref() == Some(expected),
            BypassRule::Network(network) => ip.is_some_and(|ip| network.contains(&ip)),
            BypassRule::Host {
                value,
                include_subdomains,
            } => {
                host == *value
                    || (*include_subdomains
                        && host
                            .strip_suffix(value)
                            .is_some_and(|prefix| prefix.ends_with('.')))
            }
            BypassRule::Pattern(pattern) => wildcard_matches(pattern, &host),
        })
    }
}

fn parse_ip_network(value: &str) -> Option<IpNet> {
    if let Ok(network) = value.parse::<IpNet>() {
        return Some(network);
    }
    let (address, prefix) = value.split_once('/')?;
    if address.contains(':') {
        return None;
    }
    let mut octets = address
        .split('.')
        .map(str::parse::<u8>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if octets.is_empty() || octets.len() > 4 {
        return None;
    }
    octets.resize(4, 0);
    let address = Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]);
    format!("{address}/{prefix}").parse().ok()
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star_index, mut star_value_index) = (None, 0);
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_value_index = value_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}
