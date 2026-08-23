use reqwest::blocking::{Client, ClientBuilder};

pub fn blocking_client_builder() -> ClientBuilder {
    platform::configure(Client::builder())
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use reqwest::blocking::ClientBuilder;

    pub(super) fn configure(builder: ClientBuilder) -> ClientBuilder {
        builder
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::{
        ffi::{CStr, CString},
        io::Read,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        os::raw::{c_char, c_int},
        sync::{Mutex, OnceLock},
        time::Duration,
    };

    use ipnet::IpNet;
    use reqwest::{blocking::ClientBuilder, Proxy, Url};
    use system_configuration::{
        core_foundation::{
            array::CFArray,
            base::{CFType, TCFType},
            dictionary::CFDictionary,
            number::CFNumber,
            string::{CFString, CFStringRef},
        },
        dynamic_store::SCDynamicStoreBuilder,
        sys::schema_definitions::{
            kSCPropNetProxiesExceptionsList, kSCPropNetProxiesExcludeSimpleHostnames,
            kSCPropNetProxiesHTTPEnable, kSCPropNetProxiesHTTPPort, kSCPropNetProxiesHTTPProxy,
            kSCPropNetProxiesHTTPSEnable, kSCPropNetProxiesHTTPSPort, kSCPropNetProxiesHTTPSProxy,
            kSCPropNetProxiesProxyAutoConfigEnable, kSCPropNetProxiesProxyAutoConfigJavaScript,
            kSCPropNetProxiesProxyAutoConfigURLString,
        },
    };

    type ProxySettings = CFDictionary<CFString, CFType>;

    const MAX_PAC_BYTES: u64 = 4 * 1024 * 1024;
    const PAC_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
    static PROXY_RESOLVER: OnceLock<Option<ProxyResolver>> = OnceLock::new();

    unsafe extern "C" {
        fn pacparser_init() -> c_int;
        fn pacparser_parse_pac_string(script: *const c_char) -> c_int;
        fn pacparser_find_proxy(url: *const c_char, host: *const c_char) -> *const c_char;
        fn pacparser_cleanup();
    }

    pub(super) fn configure(builder: ClientBuilder) -> ClientBuilder {
        let builder = builder.no_proxy();
        let Some(resolver) = PROXY_RESOLVER.get_or_init(ProxyResolver::from_sources) else {
            return builder;
        };
        builder.proxy(Proxy::custom(move |url| resolver.proxy_for_url(url)))
    }

    struct ProxyResolver {
        environment: EnvironmentProxyConfig,
        system: Option<SystemProxyResolver>,
    }

    impl ProxyResolver {
        fn from_sources() -> Option<Self> {
            let environment = EnvironmentProxyConfig::from_env();
            let system = if environment.covers_http_and_https() {
                None
            } else {
                SystemProxyResolver::from_settings()
            };
            if !environment.has_proxy() && system.is_none() {
                return None;
            }
            Some(Self {
                environment,
                system,
            })
        }

        fn proxy_for_url(&self, url: &Url) -> Option<String> {
            if let Some(proxy) = self.environment.proxy_for_scheme(url.scheme()) {
                return (!self.environment.bypass.matches(url)).then(|| proxy.to_owned());
            }
            self.system.as_ref()?.proxy_for_url(url)
        }
    }

    #[derive(Default)]
    struct EnvironmentProxyConfig {
        http: Option<String>,
        https: Option<String>,
        all: Option<String>,
        bypass: BypassRules,
    }

    impl EnvironmentProxyConfig {
        fn from_env() -> Self {
            let http_names: &[&str] = if std::env::var_os("REQUEST_METHOD").is_some() {
                &["http_proxy"]
            } else {
                &["HTTP_PROXY", "http_proxy"]
            };
            Self {
                http: proxy_from_env(http_names),
                https: proxy_from_env(&["HTTPS_PROXY", "https_proxy"]),
                all: proxy_from_env(&["ALL_PROXY", "all_proxy"]),
                bypass: first_env(&["NO_PROXY", "no_proxy"])
                    .map(|value| BypassRules::from_comma_list(&value, false))
                    .unwrap_or_default(),
            }
        }

        fn has_proxy(&self) -> bool {
            self.http.is_some() || self.https.is_some() || self.all.is_some()
        }

        fn covers_http_and_https(&self) -> bool {
            self.all.is_some() || (self.http.is_some() && self.https.is_some())
        }

        fn proxy_for_scheme(&self, scheme: &str) -> Option<&str> {
            match scheme {
                "http" => self.http.as_deref().or(self.all.as_deref()),
                "https" => self.https.as_deref().or(self.all.as_deref()),
                _ => None,
            }
        }
    }

    fn proxy_from_env(names: &[&str]) -> Option<String> {
        let value = first_env(names)?;
        match normalize_proxy_url(&value, None) {
            Some(proxy) => Some(proxy),
            None => {
                crate::app_warn!("http", "proxy environment variable is invalid");
                None
            }
        }
    }

    fn first_env(names: &[&str]) -> Option<String> {
        names.iter().find_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
    }

    struct SystemProxyResolver {
        bypass: BypassRules,
        mode: SystemProxyMode,
    }

    enum SystemProxyMode {
        Pac(PacResolver),
        Static {
            http: Option<String>,
            https: Option<String>,
        },
    }

    impl SystemProxyResolver {
        fn from_settings() -> Option<Self> {
            let store = SCDynamicStoreBuilder::new("OpenQuota").build()?;
            let settings = store.get_proxies()?;
            let bypass = BypassRules::from_system_settings(&settings);

            if flag_setting(&settings, unsafe { kSCPropNetProxiesProxyAutoConfigEnable }) {
                match system_pac_script(&settings)
                    .ok()
                    .and_then(|script| PacResolver::new(&script))
                {
                    Some(resolver) => {
                        crate::app_info!("http", "macOS PAC proxy enabled");
                        return Some(Self {
                            bypass,
                            mode: SystemProxyMode::Pac(resolver),
                        });
                    }
                    None => crate::app_warn!("http", "macOS PAC proxy unavailable"),
                }
            }

            let http = static_proxy_setting(
                &settings,
                unsafe { kSCPropNetProxiesHTTPEnable },
                unsafe { kSCPropNetProxiesHTTPProxy },
                unsafe { kSCPropNetProxiesHTTPPort },
            );
            let https = static_proxy_setting(
                &settings,
                unsafe { kSCPropNetProxiesHTTPSEnable },
                unsafe { kSCPropNetProxiesHTTPSProxy },
                unsafe { kSCPropNetProxiesHTTPSPort },
            );
            if http.is_none() && https.is_none() {
                return None;
            }
            crate::app_info!("http", "macOS static proxy enabled");
            Some(Self {
                bypass,
                mode: SystemProxyMode::Static { http, https },
            })
        }

        fn proxy_for_url(&self, url: &Url) -> Option<String> {
            if self.bypass.matches(url) {
                return None;
            }
            match &self.mode {
                SystemProxyMode::Pac(resolver) => resolver.proxy_for_url(url),
                SystemProxyMode::Static { http, https } => match url.scheme() {
                    "http" => http.clone(),
                    "https" => https.clone(),
                    _ => None,
                },
            }
        }
    }

    fn static_proxy_setting(
        settings: &ProxySettings,
        enabled_key: CFStringRef,
        host_key: CFStringRef,
        port_key: CFStringRef,
    ) -> Option<String> {
        if !flag_setting(settings, enabled_key) {
            return None;
        }
        let host = string_setting(settings, host_key)?;
        let port = number_setting(settings, port_key).and_then(|value| u16::try_from(value).ok());
        normalize_proxy_url(&host, port)
    }

    fn normalize_proxy_url(value: &str, port: Option<u16>) -> Option<String> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let candidate = if value.contains("://") {
            value.to_owned()
        } else if value.parse::<Ipv6Addr>().is_ok() {
            format!("http://[{value}]")
        } else {
            format!("http://{value}")
        };
        let mut url = Url::parse(&candidate).ok()?;
        if !matches!(
            url.scheme(),
            "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
        ) || url.host_str().is_none()
        {
            return None;
        }
        if let Some(port) = port {
            url.set_port(Some(port)).ok()?;
        }
        Some(url.into())
    }

    fn system_pac_script(settings: &ProxySettings) -> Result<String, &'static str> {
        if let Some(script) = string_setting(settings, unsafe {
            kSCPropNetProxiesProxyAutoConfigJavaScript
        }) {
            return Ok(script);
        }
        let Some(url) = string_setting(settings, unsafe {
            kSCPropNetProxiesProxyAutoConfigURLString
        }) else {
            return Err("automatic proxy configuration has no script or URL");
        };
        load_pac_url(&url)
    }

    fn flag_setting(settings: &ProxySettings, key: CFStringRef) -> bool {
        number_setting(settings, key).unwrap_or(0) == 1
    }

    fn number_setting(settings: &ProxySettings, key: CFStringRef) -> Option<i32> {
        settings
            .find(key)
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|value| value.to_i32())
    }

    fn string_setting(settings: &ProxySettings, key: CFStringRef) -> Option<String> {
        settings
            .find(key)
            .and_then(|value| value.downcast::<CFString>())
            .map(|value| value.to_string())
            .filter(|value| !value.trim().is_empty())
    }

    fn string_array_setting(settings: &ProxySettings, key: CFStringRef) -> Vec<String> {
        settings
            .find(key)
            .and_then(|value| value.downcast::<CFArray>())
            .map(|values| {
                values
                    .get_all_values()
                    .into_iter()
                    .map(|value| unsafe { CFType::wrap_under_get_rule(value) })
                    .filter_map(|value| value.downcast::<CFString>())
                    .map(|value| value.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[derive(Default)]
    struct BypassRules {
        exclude_simple_hostnames: bool,
        rules: Vec<BypassRule>,
    }

    enum BypassRule {
        All,
        Ip(IpAddr),
        Network(IpNet),
        Host(String),
        Pattern(String),
    }

    impl BypassRules {
        fn from_system_settings(settings: &ProxySettings) -> Self {
            let entries =
                string_array_setting(settings, unsafe { kSCPropNetProxiesExceptionsList });
            let exclude_simple =
                flag_setting(settings, unsafe { kSCPropNetProxiesExcludeSimpleHostnames });
            Self::from_entries(entries.iter().map(String::as_str), exclude_simple)
        }

        fn from_comma_list(value: &str, exclude_simple: bool) -> Self {
            Self::from_entries(value.split(','), exclude_simple)
        }

        fn from_entries<'a>(
            entries: impl IntoIterator<Item = &'a str>,
            exclude_simple: bool,
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
                        result
                            .rules
                            .push(BypassRule::Host(raw.trim_start_matches('.').to_owned()));
                    }
                }
            }
            result
        }

        fn matches(&self, url: &Url) -> bool {
            let Some(host) = url.host_str() else {
                return false;
            };
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            if self.exclude_simple_hostnames
                && !host.contains('.')
                && host.parse::<IpAddr>().is_err()
            {
                return true;
            }
            let ip = host.parse::<IpAddr>().ok();
            self.rules.iter().any(|rule| match rule {
                BypassRule::All => true,
                BypassRule::Ip(expected) => ip.as_ref() == Some(expected),
                BypassRule::Network(network) => ip.is_some_and(|ip| network.contains(&ip)),
                BypassRule::Host(expected) => {
                    host == *expected
                        || host
                            .strip_suffix(expected)
                            .is_some_and(|prefix| prefix.ends_with('.'))
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

    fn load_pac_url(value: &str) -> Result<String, &'static str> {
        let url = Url::parse(value).map_err(|_| "automatic proxy URL is invalid")?;
        match url.scheme() {
            "http" | "https" => load_http_pac(url),
            "file" => {
                let path = url
                    .to_file_path()
                    .map_err(|_| "automatic proxy file URL is invalid")?;
                let metadata = std::fs::metadata(&path)
                    .map_err(|_| "automatic proxy file could not be read")?;
                if metadata.len() > MAX_PAC_BYTES {
                    return Err("automatic proxy script is too large");
                }
                std::fs::read_to_string(path).map_err(|_| "automatic proxy file is not valid UTF-8")
            }
            _ => Err("automatic proxy URL scheme is unsupported"),
        }
    }

    fn load_http_pac(url: Url) -> Result<String, &'static str> {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .connect_timeout(PAC_FETCH_TIMEOUT)
            .timeout(PAC_FETCH_TIMEOUT)
            .build()
            .map_err(|_| "automatic proxy downloader could not be created")?;
        let response = client
            .get(url)
            .send()
            .map_err(|_| "automatic proxy URL could not be reached")?;
        if !response.status().is_success() {
            return Err("automatic proxy URL returned an error");
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PAC_BYTES)
        {
            return Err("automatic proxy script is too large");
        }
        let mut script = String::new();
        let bytes_read = response
            .take(MAX_PAC_BYTES + 1)
            .read_to_string(&mut script)
            .map_err(|_| "automatic proxy response is not valid UTF-8")?;
        if bytes_read as u64 > MAX_PAC_BYTES {
            return Err("automatic proxy script is too large");
        }
        Ok(script)
    }

    struct PacResolver {
        engine: Mutex<()>,
    }

    impl PacResolver {
        fn new(script: &str) -> Option<Self> {
            let script = CString::new(script).ok()?;
            if unsafe { pacparser_init() } != 1 {
                return None;
            }
            if unsafe { pacparser_parse_pac_string(script.as_ptr()) } != 1 {
                unsafe { pacparser_cleanup() };
                return None;
            }
            Some(Self {
                engine: Mutex::new(()),
            })
        }

        fn proxy_for_url(&self, url: &Url) -> Option<String> {
            let host = url.host_str()?;
            let url = CString::new(url.as_str()).ok()?;
            let host = CString::new(host).ok()?;
            let _guard = self.engine.lock().ok()?;
            let result = unsafe { pacparser_find_proxy(url.as_ptr(), host.as_ptr()) };
            if result.is_null() {
                return None;
            }
            let result = unsafe { CStr::from_ptr(result) }.to_str().ok()?;
            proxy_url_from_pac(result)
        }
    }

    fn proxy_url_from_pac(result: &str) -> Option<String> {
        for directive in result
            .split(';')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let mut fields = directive.split_whitespace();
            let kind = fields.next()?.to_ascii_uppercase();
            if kind == "DIRECT" {
                return None;
            }
            let Some(address) = fields.next() else {
                continue;
            };
            if fields.next().is_some() {
                continue;
            }
            let scheme = match kind.as_str() {
                "PROXY" | "HTTP" => "http",
                "HTTPS" => "https",
                "SOCKS" | "SOCKS4" => "socks4a",
                "SOCKS5" => "socks5h",
                _ => continue,
            };
            let proxy = format!("{scheme}://{address}");
            if Url::parse(&proxy).is_ok() {
                return Some(proxy);
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::{
            proxy_url_from_pac, BypassRules, EnvironmentProxyConfig, ProxyResolver,
            SystemProxyMode, SystemProxyResolver,
        };
        use reqwest::Url;

        fn url(value: &str) -> Url {
            Url::parse(value).unwrap()
        }

        #[test]
        fn environment_proxy_is_terminal_for_its_scheme() {
            let resolver = ProxyResolver {
                environment: EnvironmentProxyConfig {
                    http: Some("http://environment:8080/".into()),
                    https: None,
                    all: None,
                    bypass: BypassRules::from_comma_list("direct.example", false),
                },
                system: Some(SystemProxyResolver {
                    bypass: BypassRules::default(),
                    mode: SystemProxyMode::Static {
                        http: Some("http://system-http:8080/".into()),
                        https: Some("http://system-https:8443/".into()),
                    },
                }),
            };

            assert_eq!(
                resolver.proxy_for_url(&url("http://proxied.example")),
                Some("http://environment:8080/".into())
            );
            assert_eq!(resolver.proxy_for_url(&url("http://direct.example")), None);
            assert_eq!(
                resolver.proxy_for_url(&url("https://proxied.example")),
                Some("http://system-https:8443/".into())
            );
        }

        #[test]
        fn system_bypass_applies_only_to_system_proxy() {
            let resolver = ProxyResolver {
                environment: EnvironmentProxyConfig {
                    http: Some("http://environment:8080/".into()),
                    https: None,
                    all: None,
                    bypass: BypassRules::default(),
                },
                system: Some(SystemProxyResolver {
                    bypass: BypassRules::from_comma_list("bypass.example", false),
                    mode: SystemProxyMode::Static {
                        http: Some("http://system-http:8080/".into()),
                        https: Some("http://system-https:8443/".into()),
                    },
                }),
            };

            assert_eq!(
                resolver.proxy_for_url(&url("http://bypass.example")),
                Some("http://environment:8080/".into())
            );
            assert_eq!(resolver.proxy_for_url(&url("https://bypass.example")), None);
        }

        #[test]
        fn bypass_rules_cover_domains_wildcards_networks_and_simple_hosts() {
            let rules = BypassRules::from_comma_list(
                ".example.com,*.internal,10.0.0.0/8,169.254/16,<local>",
                false,
            );
            for target in [
                "https://example.com",
                "https://www.example.com",
                "https://service.internal",
                "http://10.2.3.4",
                "http://169.254.2.1",
                "http://printer",
            ] {
                assert!(rules.matches(&url(target)), "{target} should bypass");
            }
            assert!(!rules.matches(&url("https://notexample.com")));
            assert!(!rules.matches(&url("https://internal.example.net")));
        }

        #[test]
        fn translates_supported_pac_directives_in_order() {
            assert_eq!(
                proxy_url_from_pac("PROXY 127.0.0.1:8118; DIRECT"),
                Some("http://127.0.0.1:8118".into())
            );
            assert_eq!(
                proxy_url_from_pac("SOCKS5 proxy.example:1080; DIRECT"),
                Some("socks5h://proxy.example:1080".into())
            );
            assert_eq!(proxy_url_from_pac("DIRECT; PROXY ignored:80"), None);
            assert_eq!(
                proxy_url_from_pac("QUIC unsupported:443; HTTPS proxy.example:8443"),
                Some("https://proxy.example:8443".into())
            );
        }

        #[test]
        fn rejects_malformed_proxy_addresses() {
            assert_eq!(proxy_url_from_pac("PROXY ; DIRECT"), None);
            assert_eq!(proxy_url_from_pac("PROXY not a url; DIRECT"), None);
        }

        #[test]
        #[ignore = "requires a configured macOS PAC service"]
        fn configured_system_pac_resolves_chatgpt() {
            let resolver =
                SystemProxyResolver::from_settings().expect("macOS system proxy should initialize");
            let target = url("https://chatgpt.com/backend-api/wham/usage");
            assert!(resolver.proxy_for_url(&target).is_some());
        }
    }
}
