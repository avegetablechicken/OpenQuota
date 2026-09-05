use reqwest::blocking::{Client, ClientBuilder};

pub fn blocking_client_builder() -> ClientBuilder {
    platform::configure(Client::builder())
}

#[cfg(target_os = "macos")]
pub fn warm_system_proxy_credentials() {
    platform::warm_system_proxy_credentials();
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
        collections::HashMap,
        ffi::{CStr, CString},
        io::Read,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        os::raw::{c_char, c_int},
        sync::{Mutex, MutexGuard, OnceLock},
        time::{Duration, Instant},
    };

    use ipnet::IpNet;
    use reqwest::{blocking::ClientBuilder, Proxy, Url};
    use security_core_foundation::{
        array::CFArray as SecurityCFArray,
        base::{CFType as SecurityCFType, TCFType as SecurityTCFType},
        boolean::CFBoolean as SecurityCFBoolean,
        dictionary::CFDictionary as SecurityCFDictionary,
        string::CFString as SecurityCFString,
    };
    use security_framework::os::macos::{
        keychain::SecKeychain,
        passwords::{find_internet_password, SecAuthenticationType, SecProtocolType},
    };
    use security_framework_sys::{
        base::errSecSuccess,
        item::{
            kSecAttrAccount, kSecAttrServer, kSecClass, kSecClassInternetPassword,
            kSecMatchSearchList, kSecReturnAttributes,
        },
        keychain_item::SecItemCopyMatching,
    };
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
    const SYSTEM_PROXY_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
    static PROXY_RESOLVER: OnceLock<ProxyResolver> = OnceLock::new();
    static PAC_ENGINE: OnceLock<Mutex<()>> = OnceLock::new();

    unsafe extern "C" {
        fn pacparser_init() -> c_int;
        fn pacparser_parse_pac_string(script: *const c_char) -> c_int;
        fn pacparser_find_proxy(url: *const c_char, host: *const c_char) -> *const c_char;
        fn pacparser_cleanup();
    }

    pub(super) fn configure(builder: ClientBuilder) -> ClientBuilder {
        let builder = builder.no_proxy();
        let resolver = PROXY_RESOLVER.get_or_init(ProxyResolver::from_sources);
        builder.proxy(Proxy::custom(move |url| resolver.proxy_for_url(url)))
    }

    pub(super) fn warm_system_proxy_credentials() {
        let Ok(url) = Url::parse("https://chatgpt.com/backend-api/wham/usage") else {
            return;
        };
        PROXY_RESOLVER
            .get_or_init(ProxyResolver::from_sources)
            .system
            .warm_credentials_for_url(&url);
    }

    struct ProxyResolver {
        environment: EnvironmentProxyConfig,
        system: DynamicSystemProxyResolver,
    }

    impl ProxyResolver {
        fn from_sources() -> Self {
            let environment = EnvironmentProxyConfig::from_env();
            Self {
                environment,
                system: DynamicSystemProxyResolver::default(),
            }
        }

        fn proxy_for_url(&self, url: &Url) -> Option<String> {
            if let Some(proxy) = self.environment.proxy_for_scheme(url.scheme()) {
                return (!self.environment.bypass.matches(url)).then(|| proxy.to_owned());
            }
            self.system.proxy_for_url(url)
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

    #[derive(Default)]
    struct DynamicSystemProxyResolver {
        cache: Mutex<SystemProxyCache>,
    }

    #[derive(Default)]
    struct SystemProxyCache {
        checked_at: Option<Instant>,
        configuration: Option<SystemProxyConfiguration>,
        resolver: Option<SystemProxyResolver>,
        reload_failed: bool,
    }

    #[derive(Clone, PartialEq, Eq)]
    struct SystemProxyConfiguration {
        pac: Option<PacSource>,
        fallback: Option<StaticProxyConfig>,
        bypass: BypassRules,
    }

    #[derive(Clone, PartialEq, Eq)]
    enum PacSource {
        Script(String),
        Url(String),
    }

    struct SystemProxyResolver {
        pac: Option<PacResolver>,
        fallback: Option<StaticProxyConfig>,
        bypass: BypassRules,
        credentials: Mutex<HashMap<ProxyEndpoint, Option<ProxyCredentials>>>,
    }

    #[derive(Clone, Hash, PartialEq, Eq)]
    struct ProxyEndpoint {
        host: String,
        port: u16,
    }

    #[derive(Clone)]
    struct ProxyCredentials {
        username: zeroize::Zeroizing<String>,
        password: zeroize::Zeroizing<String>,
    }

    #[derive(Clone, PartialEq, Eq)]
    struct StaticProxyConfig {
        bypass: BypassRules,
        http: Option<String>,
        https: Option<String>,
    }

    impl DynamicSystemProxyResolver {
        fn proxy_for_url(&self, url: &Url) -> Option<String> {
            let mut cache = self.cache.lock().ok()?;
            if cache
                .checked_at
                .is_none_or(|checked| checked.elapsed() >= SYSTEM_PROXY_REFRESH_INTERVAL)
            {
                cache.checked_at = Some(Instant::now());
                cache.update(SystemProxyConfiguration::from_settings());
            }
            cache
                .resolver
                .as_ref()
                .and_then(|resolver| resolver.proxy_for_url(url))
        }

        fn warm_credentials_for_url(&self, url: &Url) {
            let Ok(mut cache) = self.cache.lock() else {
                return;
            };
            cache.update(SystemProxyConfiguration::from_settings());
            if let Some(resolver) = &cache.resolver {
                resolver.warm_credentials_for_url(url);
            }
        }
    }

    impl SystemProxyCache {
        fn update(&mut self, configuration: Option<SystemProxyConfiguration>) {
            if self.configuration == configuration && !self.reload_failed {
                return;
            }

            // pacparser keeps one process-global engine. Replacing the resolver reloads
            // that engine under its own mutex while this cache mutex excludes evaluations.
            self.resolver = None;
            self.configuration = configuration.clone();
            let (resolver, reload_failed) = configuration
                .map(SystemProxyResolver::from_configuration)
                .unwrap_or((None, false));
            self.resolver = resolver;
            self.reload_failed = reload_failed;
        }
    }

    impl SystemProxyConfiguration {
        fn from_settings() -> Option<Self> {
            let store = SCDynamicStoreBuilder::new("OpenQuota").build()?;
            let settings = store.get_proxies()?;
            let fallback = StaticProxyConfig::from_settings(&settings);
            let bypass = BypassRules::from_system_settings(&settings);
            let pac = if flag_setting(&settings, unsafe { kSCPropNetProxiesProxyAutoConfigEnable })
            {
                pac_source(&settings)
            } else {
                None
            };
            (pac.is_some() || fallback.is_some()).then_some(Self {
                pac,
                fallback,
                bypass,
            })
        }
    }

    impl SystemProxyResolver {
        fn from_configuration(configuration: SystemProxyConfiguration) -> (Option<Self>, bool) {
            let pac_requested = configuration.pac.is_some();
            let pac = configuration.pac.and_then(|source| {
                source
                    .load()
                    .ok()
                    .and_then(|script| PacResolver::new(&script))
            });
            if pac.is_some() {
                crate::app_info!("http", "macOS PAC proxy enabled");
            } else if pac_requested {
                crate::app_warn!("http", "macOS PAC proxy unavailable");
            } else if configuration.fallback.is_some() {
                crate::app_info!("http", "macOS static proxy enabled");
            }
            let reload_failed = pac_requested && pac.is_none();
            let resolver = (pac.is_some() || configuration.fallback.is_some()).then_some(Self {
                pac,
                fallback: configuration.fallback,
                bypass: configuration.bypass,
                credentials: Mutex::new(HashMap::new()),
            });
            (resolver, reload_failed)
        }

        fn proxy_for_url(&self, url: &Url) -> Option<String> {
            if self.bypass.matches(url) {
                return None;
            }
            let proxy = if let Some(pac) = &self.pac {
                self.apply_pac_decision(url, pac.decision_for_url(url))
            } else {
                self.fallback.as_ref()?.proxy_for_url(url)
            }?;
            Some(self.proxy_with_credentials(proxy))
        }

        fn apply_pac_decision(&self, url: &Url, decision: PacDecision) -> Option<String> {
            match decision {
                PacDecision::Proxy(proxy) => Some(proxy),
                PacDecision::Direct => None,
                PacDecision::Error => {
                    crate::app_warn!("http", "macOS PAC evaluation failed; using fallback");
                    self.fallback.as_ref()?.proxy_for_url(url)
                }
            }
        }

        fn proxy_with_credentials(&self, proxy: String) -> String {
            let Ok(mut url) = Url::parse(&proxy) else {
                return proxy;
            };
            if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() {
                return proxy;
            }
            let Some(host) = url.host_str().map(str::to_owned) else {
                return proxy;
            };
            let Some(port) = url.port_or_known_default() else {
                return proxy;
            };
            let endpoint = ProxyEndpoint { host, port };
            let credentials = self
                .credentials
                .lock()
                .ok()
                .and_then(|cache| cache.get(&endpoint).cloned().flatten());
            let Some(credentials) = credentials else {
                return proxy;
            };
            if url.set_username(&credentials.username).is_err()
                || url.set_password(Some(&credentials.password)).is_err()
            {
                return proxy;
            }
            url.into()
        }

        fn warm_credentials_for_url(&self, url: &Url) {
            if self.bypass.matches(url) {
                return;
            }
            let proxy = if let Some(pac) = &self.pac {
                self.apply_pac_decision(url, pac.decision_for_url(url))
            } else {
                self.fallback
                    .as_ref()
                    .and_then(|proxy| proxy.proxy_for_url(url))
            };
            let Some(proxy) = proxy.and_then(|proxy| Url::parse(&proxy).ok()) else {
                return;
            };
            if !matches!(proxy.scheme(), "http" | "https") || !proxy.username().is_empty() {
                return;
            }
            let Some(host) = proxy.host_str().map(str::to_owned) else {
                return;
            };
            let Some(port) = proxy.port_or_known_default() else {
                return;
            };
            let endpoint = ProxyEndpoint { host, port };
            let credentials = load_proxy_credentials(&endpoint);
            if let Ok(mut cache) = self.credentials.lock() {
                cache.insert(endpoint, credentials);
            }
        }
    }

    impl PacSource {
        fn load(self) -> Result<String, &'static str> {
            match self {
                Self::Script(script) => Ok(script),
                Self::Url(url) => std::thread::spawn(move || load_pac_url(&url))
                    .join()
                    .map_err(|_| "automatic proxy downloader stopped unexpectedly")?,
            }
        }
    }

    fn load_proxy_credentials(endpoint: &ProxyEndpoint) -> Option<ProxyCredentials> {
        let keychain = SecKeychain::default().ok()?;
        let keychains = SecurityCFArray::from_CFTypes(std::slice::from_ref(&keychain));
        // macOS may save one credential for companion HTTP/SOCKS ports. Match the
        // exact proxy server and let Keychain select its default internet password.
        let query = unsafe {
            SecurityCFDictionary::from_CFType_pairs(&[
                (
                    SecurityCFString::wrap_under_get_rule(kSecClass),
                    SecurityCFString::wrap_under_get_rule(kSecClassInternetPassword).into_CFType(),
                ),
                (
                    SecurityCFString::wrap_under_get_rule(kSecAttrServer),
                    SecurityCFString::from(endpoint.host.as_str()).into_CFType(),
                ),
                (
                    SecurityCFString::wrap_under_get_rule(kSecReturnAttributes),
                    SecurityCFBoolean::true_value().into_CFType(),
                ),
                (
                    SecurityCFString::wrap_under_get_rule(kSecMatchSearchList),
                    keychains.into_CFType(),
                ),
            ])
        };
        let mut result = std::ptr::null();
        let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result) };
        if status != errSecSuccess || result.is_null() {
            return None;
        }
        let item = unsafe {
            SecurityCFDictionary::<SecurityCFString, SecurityCFType>::wrap_under_create_rule(
                result.cast_mut().cast(),
            )
        };
        let username = item
            .find(unsafe { kSecAttrAccount })?
            .downcast::<SecurityCFString>()?
            .to_string();
        let password_result = find_internet_password(
            Some(std::slice::from_ref(&keychain)),
            &endpoint.host,
            None,
            &username,
            "",
            None,
            SecProtocolType::Any,
            SecAuthenticationType::Any,
        );
        let (password, _) = password_result.ok()?;
        let password = String::from_utf8(password.as_ref().to_vec()).ok()?;
        if username.is_empty() || password.is_empty() {
            return None;
        }
        crate::app_info!("http", "macOS proxy credentials loaded");
        Some(ProxyCredentials {
            username: zeroize::Zeroizing::new(username),
            password: zeroize::Zeroizing::new(password),
        })
    }

    impl StaticProxyConfig {
        fn from_settings(settings: &ProxySettings) -> Option<Self> {
            let http = static_proxy_setting(
                settings,
                unsafe { kSCPropNetProxiesHTTPEnable },
                unsafe { kSCPropNetProxiesHTTPProxy },
                unsafe { kSCPropNetProxiesHTTPPort },
            );
            let https = static_proxy_setting(
                settings,
                unsafe { kSCPropNetProxiesHTTPSEnable },
                unsafe { kSCPropNetProxiesHTTPSProxy },
                unsafe { kSCPropNetProxiesHTTPSPort },
            );
            if http.is_none() && https.is_none() {
                return None;
            }
            Some(Self {
                bypass: BypassRules::from_system_settings(settings),
                http,
                https,
            })
        }

        fn proxy_for_url(&self, url: &Url) -> Option<String> {
            if self.bypass.matches(url) {
                return None;
            }
            match url.scheme() {
                "http" => self.http.clone(),
                "https" => self.https.clone(),
                _ => None,
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

    fn pac_source(settings: &ProxySettings) -> Option<PacSource> {
        if let Some(script) = string_setting(settings, unsafe {
            kSCPropNetProxiesProxyAutoConfigJavaScript
        }) {
            return Some(PacSource::Script(script));
        }
        string_setting(settings, unsafe {
            kSCPropNetProxiesProxyAutoConfigURLString
        })
        .map(PacSource::Url)
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

    #[derive(Clone, Default, PartialEq, Eq)]
    struct BypassRules {
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
    enum HostMatch {
        Exact,
        IncludeSubdomains,
    }

    impl BypassRules {
        fn from_system_settings(settings: &ProxySettings) -> Self {
            let entries =
                string_array_setting(settings, unsafe { kSCPropNetProxiesExceptionsList });
            let exclude_simple =
                flag_setting(settings, unsafe { kSCPropNetProxiesExcludeSimpleHostnames });
            Self::from_entries(
                entries.iter().map(String::as_str),
                exclude_simple,
                HostMatch::Exact,
            )
        }

        fn from_comma_list(value: &str, exclude_simple: bool) -> Self {
            Self::from_entries(
                value.split(','),
                exclude_simple,
                HostMatch::IncludeSubdomains,
            )
        }

        fn from_entries<'a>(
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

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum PacDecision {
        Proxy(String),
        Direct,
        Error,
    }

    struct PacResolver {
        script: CString,
        decisions: Mutex<HashMap<String, PacDecision>>,
    }

    struct PacParserSession {
        _guard: MutexGuard<'static, ()>,
    }

    impl PacResolver {
        fn new(script: &str) -> Option<Self> {
            let script = CString::new(script).ok()?;
            PacParserSession::new(&script)?;
            Some(Self {
                script,
                decisions: Mutex::new(HashMap::new()),
            })
        }

        fn decision_for_url(&self, url: &Url) -> PacDecision {
            let cache_key = url.as_str();
            let Ok(mut decisions) = self.decisions.lock() else {
                return PacDecision::Error;
            };
            if let Some(decision) = decisions.get(cache_key) {
                return decision.clone();
            }
            let Some(host) = url.host_str() else {
                return PacDecision::Error;
            };
            let Ok(url) = CString::new(url.as_str()) else {
                return PacDecision::Error;
            };
            let Ok(host) = CString::new(host) else {
                return PacDecision::Error;
            };
            let Some(_session) = PacParserSession::new(&self.script) else {
                return PacDecision::Error;
            };
            let result = unsafe { pacparser_find_proxy(url.as_ptr(), host.as_ptr()) };
            if result.is_null() {
                return PacDecision::Error;
            }
            let Ok(result) = unsafe { CStr::from_ptr(result) }.to_str() else {
                return PacDecision::Error;
            };
            let decision = pac_decision_from_result(result);
            if decision != PacDecision::Error {
                decisions.insert(cache_key.to_owned(), decision.clone());
            }
            decision
        }
    }

    impl PacParserSession {
        fn new(script: &CString) -> Option<Self> {
            let guard = PAC_ENGINE.get_or_init(|| Mutex::new(())).lock().ok()?;
            if unsafe { pacparser_init() } != 1 {
                return None;
            }
            if unsafe { pacparser_parse_pac_string(script.as_ptr()) } != 1 {
                unsafe { pacparser_cleanup() };
                return None;
            }
            Some(Self { _guard: guard })
        }
    }

    impl Drop for PacParserSession {
        fn drop(&mut self) {
            unsafe { pacparser_cleanup() };
        }
    }

    fn pac_decision_from_result(result: &str) -> PacDecision {
        for directive in result
            .split(';')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let mut fields = directive.split_whitespace();
            let Some(kind) = fields.next().map(str::to_ascii_uppercase) else {
                continue;
            };
            if kind == "DIRECT" {
                return PacDecision::Direct;
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
                return PacDecision::Proxy(proxy);
            }
        }
        PacDecision::Error
    }

    #[cfg(test)]
    mod tests {
        use super::{
            pac_decision_from_result, BypassRules, DynamicSystemProxyResolver,
            EnvironmentProxyConfig, HostMatch, PacDecision, ProxyCredentials, ProxyEndpoint,
            ProxyResolver, StaticProxyConfig, SystemProxyCache, SystemProxyConfiguration,
            SystemProxyResolver,
        };
        use reqwest::Url;
        use std::{collections::HashMap, sync::Mutex, time::Instant};

        fn url(value: &str) -> Url {
            Url::parse(value).unwrap()
        }

        fn dynamic_system(resolver: SystemProxyResolver) -> DynamicSystemProxyResolver {
            DynamicSystemProxyResolver {
                cache: Mutex::new(SystemProxyCache {
                    checked_at: Some(Instant::now()),
                    resolver: Some(resolver),
                    ..SystemProxyCache::default()
                }),
            }
        }

        fn static_configuration(https: Option<&str>) -> SystemProxyConfiguration {
            SystemProxyConfiguration {
                pac: None,
                fallback: Some(StaticProxyConfig {
                    bypass: BypassRules::default(),
                    http: None,
                    https: https.map(str::to_owned),
                }),
                bypass: BypassRules::default(),
            }
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
                system: dynamic_system(SystemProxyResolver {
                    pac: None,
                    fallback: Some(StaticProxyConfig {
                        bypass: BypassRules::default(),
                        http: Some("http://system-http:8080/".into()),
                        https: Some("http://system-https:8443/".into()),
                    }),
                    bypass: BypassRules::default(),
                    credentials: Mutex::new(HashMap::new()),
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
                system: dynamic_system(SystemProxyResolver {
                    pac: None,
                    fallback: Some(StaticProxyConfig {
                        bypass: BypassRules::from_entries(
                            ["bypass.example"],
                            false,
                            HostMatch::Exact,
                        ),
                        http: Some("http://system-http:8080/".into()),
                        https: Some("http://system-https:8443/".into()),
                    }),
                    bypass: BypassRules::from_entries(["bypass.example"], false, HostMatch::Exact),
                    credentials: Mutex::new(HashMap::new()),
                }),
            };

            assert_eq!(
                resolver.proxy_for_url(&url("http://bypass.example")),
                Some("http://environment:8080/".into())
            );
            assert_eq!(resolver.proxy_for_url(&url("https://bypass.example")), None);
        }

        #[test]
        fn system_proxy_cache_applies_configuration_changes() {
            let target = url("https://example.com");
            let mut cache = SystemProxyCache::default();

            cache.update(Some(static_configuration(Some("http://first:8080"))));
            assert_eq!(
                cache
                    .resolver
                    .as_ref()
                    .and_then(|resolver| resolver.proxy_for_url(&target)),
                Some("http://first:8080".into())
            );

            cache.update(Some(static_configuration(Some("http://second:8080"))));
            assert_eq!(
                cache
                    .resolver
                    .as_ref()
                    .and_then(|resolver| resolver.proxy_for_url(&target)),
                Some("http://second:8080".into())
            );

            cache.update(None);
            assert!(cache.resolver.is_none());
        }

        #[test]
        fn system_proxy_uses_cached_basic_credentials() {
            let endpoint = ProxyEndpoint {
                host: "proxy.example".into(),
                port: 8080,
            };
            let resolver = SystemProxyResolver {
                pac: None,
                fallback: None,
                bypass: BypassRules::default(),
                credentials: Mutex::new(HashMap::from([(
                    endpoint,
                    Some(ProxyCredentials {
                        username: zeroize::Zeroizing::new("user@example".into()),
                        password: zeroize::Zeroizing::new("p@ss/word".into()),
                    }),
                )])),
            };

            assert_eq!(
                resolver.proxy_with_credentials("http://proxy.example:8080".into()),
                "http://user%40example:p%40ss%2Fword@proxy.example:8080/"
            );
        }

        #[test]
        fn pac_direct_is_terminal_and_only_errors_use_static_fallback() {
            let resolver = SystemProxyResolver {
                pac: None,
                fallback: Some(StaticProxyConfig {
                    bypass: BypassRules::default(),
                    http: Some("http://system-http:8080/".into()),
                    https: Some("http://system-https:8443/".into()),
                }),
                bypass: BypassRules::default(),
                credentials: Mutex::new(HashMap::new()),
            };
            let target = url("https://example.com");
            assert_eq!(
                resolver.apply_pac_decision(
                    &target,
                    PacDecision::Proxy("http://pac-proxy:8118/".into())
                ),
                Some("http://pac-proxy:8118/".into())
            );
            assert_eq!(
                resolver.apply_pac_decision(&target, PacDecision::Direct),
                None
            );
            assert_eq!(
                resolver.apply_pac_decision(&target, PacDecision::Error),
                Some("http://system-https:8443/".into())
            );
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
        fn system_plain_hosts_are_exact_and_wildcards_select_subdomains() {
            let rules =
                BypassRules::from_entries(["example.com", "*.internal"], false, HostMatch::Exact);
            assert!(rules.matches(&url("https://example.com")));
            assert!(!rules.matches(&url("https://www.example.com")));
            assert!(rules.matches(&url("https://service.internal")));
        }

        #[test]
        fn translates_supported_pac_directives_in_order() {
            assert_eq!(
                pac_decision_from_result("PROXY 127.0.0.1:8118; DIRECT"),
                PacDecision::Proxy("http://127.0.0.1:8118".into())
            );
            assert_eq!(
                pac_decision_from_result("SOCKS5 proxy.example:1080; DIRECT"),
                PacDecision::Proxy("socks5h://proxy.example:1080".into())
            );
            assert_eq!(
                pac_decision_from_result("DIRECT; PROXY ignored:80"),
                PacDecision::Direct
            );
            assert_eq!(
                pac_decision_from_result("QUIC unsupported:443; HTTPS proxy.example:8443"),
                PacDecision::Proxy("https://proxy.example:8443".into())
            );
        }

        #[test]
        fn rejects_malformed_proxy_addresses() {
            assert_eq!(
                pac_decision_from_result("PROXY ; DIRECT"),
                PacDecision::Direct
            );
            assert_eq!(
                pac_decision_from_result("PROXY not a url"),
                PacDecision::Error
            );
        }

        #[test]
        #[ignore = "requires a configured macOS PAC service"]
        fn configured_system_pac_resolves_chatgpt() {
            let configuration = SystemProxyConfiguration::from_settings()
                .expect("macOS system proxy should initialize");
            let resolver = SystemProxyResolver::from_configuration(configuration)
                .0
                .expect("macOS system proxy should resolve");
            let target = url("https://chatgpt.com/backend-api/wham/usage");
            assert!(resolver.proxy_for_url(&target).is_some());
        }

        #[test]
        #[ignore = "requires an authenticated macOS proxy with Keychain credentials"]
        fn configured_system_proxy_loads_keychain_credentials() {
            let configuration = SystemProxyConfiguration::from_settings()
                .expect("macOS system proxy should initialize");
            let resolver = SystemProxyResolver::from_configuration(configuration)
                .0
                .expect("macOS system proxy should resolve");
            let target = url("https://chatgpt.com/backend-api/wham/usage");
            resolver.warm_credentials_for_url(&target);
            let proxy = resolver
                .proxy_for_url(&target)
                .expect("ChatGPT should use the configured proxy");
            let proxy = url(&proxy);
            assert!(!proxy.username().is_empty());
            assert!(proxy
                .password()
                .is_some_and(|password| !password.is_empty()));
        }
    }
}
