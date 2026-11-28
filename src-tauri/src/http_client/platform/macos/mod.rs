mod bypass;
mod pac;

use std::{
    collections::HashMap,
    net::Ipv6Addr,
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};

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
        kSecAttrAccount, kSecAttrServer, kSecClass, kSecClassInternetPassword, kSecMatchSearchList,
        kSecReturnAttributes,
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
        kSCPropNetProxiesHTTPEnable, kSCPropNetProxiesHTTPPort, kSCPropNetProxiesHTTPProxy,
        kSCPropNetProxiesHTTPSEnable, kSCPropNetProxiesHTTPSPort, kSCPropNetProxiesHTTPSProxy,
        kSCPropNetProxiesProxyAutoConfigEnable,
    },
};

use bypass::BypassRules;
#[cfg(test)]
use bypass::HostMatch;
#[cfg(test)]
use pac::pac_decision_from_result;
use pac::{pac_source, PacDecision, PacResolver, PacSource};

type ProxySettings = CFDictionary<CFString, CFType>;

const SYSTEM_PROXY_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
// A PAC URL that cannot be loaded is retried with backoff: each retry blocks
// proxied requests on the cache mutex for up to the PAC download timeout.
const PAC_RELOAD_INITIAL_BACKOFF: Duration = Duration::from_secs(15);
const PAC_RELOAD_MAX_BACKOFF: Duration = Duration::from_secs(300);
static PROXY_RESOLVER: OnceLock<ProxyResolver> = OnceLock::new();

pub(in crate::http_client) fn configure(builder: ClientBuilder) -> ClientBuilder {
    let builder = builder.no_proxy();
    let resolver = PROXY_RESOLVER.get_or_init(ProxyResolver::from_sources);
    builder.proxy(Proxy::custom(move |url| resolver.proxy_for_url(url)))
}

pub(in crate::http_client) fn configure_environment(
    builder: ClientBuilder,
    read: impl Fn(&[&str]) -> Option<String>,
) -> ClientBuilder {
    let resolver = ProxyResolver {
        environment: EnvironmentProxyConfig::from_reader(read),
        system: DynamicSystemProxyResolver::default(),
    };
    builder
        .no_proxy()
        .proxy(Proxy::custom(move |url| resolver.proxy_for_url(url)))
}

struct ProxyResolver {
    environment: EnvironmentProxyConfig,
    system: DynamicSystemProxyResolver,
}

impl ProxyResolver {
    fn from_sources() -> Self {
        let environment = EnvironmentProxyConfig::from_reader(first_env);
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
    fn from_reader(read: impl Fn(&[&str]) -> Option<String>) -> Self {
        let http_names: &[&str] = if std::env::var_os("REQUEST_METHOD").is_some() {
            &["http_proxy"]
        } else {
            &["HTTP_PROXY", "http_proxy"]
        };
        Self {
            http: proxy_from_env(http_names, &read),
            https: proxy_from_env(&["HTTPS_PROXY", "https_proxy"], &read),
            all: proxy_from_env(&["ALL_PROXY", "all_proxy"], &read),
            bypass: read(&["NO_PROXY", "no_proxy"])
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

fn proxy_from_env(names: &[&str], read: impl Fn(&[&str]) -> Option<String>) -> Option<String> {
    let value = read(names)?;
    if value.trim().is_empty() {
        return None;
    }
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
    resolver: Option<Arc<SystemProxyResolver>>,
    reload_retry: Option<PacReloadRetry>,
}

#[derive(Clone, Copy)]
struct PacReloadRetry {
    at: Instant,
    backoff: Duration,
}

#[derive(Clone, PartialEq, Eq)]
struct SystemProxyConfiguration {
    pac: Option<PacSource>,
    fallback: Option<StaticProxyConfig>,
    bypass: BypassRules,
}

#[derive(Clone)]
enum CredentialEntry {
    Loading,
    Loaded(Option<ProxyCredentials>),
}

#[derive(Default)]
struct CredentialCache {
    entries: Mutex<HashMap<ProxyEndpoint, CredentialEntry>>,
    ready: Condvar,
    #[cfg(test)]
    waiters: std::sync::atomic::AtomicUsize,
}

impl CredentialCache {
    fn get_or_load(
        &self,
        endpoint: ProxyEndpoint,
        load: impl FnOnce(&ProxyEndpoint) -> Option<ProxyCredentials>,
    ) -> Option<ProxyCredentials> {
        let mut entries = self.entries.lock().ok()?;
        loop {
            match entries.get(&endpoint) {
                Some(CredentialEntry::Loaded(credentials)) => return credentials.clone(),
                Some(CredentialEntry::Loading) => {
                    #[cfg(test)]
                    self.waiters
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let waited = self.ready.wait(entries);
                    #[cfg(test)]
                    self.waiters
                        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    entries = waited.ok()?;
                }
                None => {
                    entries.insert(endpoint.clone(), CredentialEntry::Loading);
                    drop(entries);
                    break;
                }
            }
        }

        struct LoadingGuard<'a> {
            cache: &'a CredentialCache,
            endpoint: Option<ProxyEndpoint>,
        }
        impl Drop for LoadingGuard<'_> {
            fn drop(&mut self) {
                if let Some(endpoint) = self.endpoint.take() {
                    if let Ok(mut entries) = self.cache.entries.lock() {
                        if matches!(entries.get(&endpoint), Some(CredentialEntry::Loading)) {
                            entries.remove(&endpoint);
                        }
                        self.cache.ready.notify_all();
                    }
                }
            }
        }

        let mut guard = LoadingGuard {
            cache: self,
            endpoint: Some(endpoint.clone()),
        };
        let loaded = load(&endpoint);
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(endpoint, CredentialEntry::Loaded(loaded.clone()));
            guard.endpoint = None;
            self.ready.notify_all();
        }
        loaded
    }
}

struct SystemProxyResolver {
    pac: Option<PacResolver>,
    fallback: Option<StaticProxyConfig>,
    bypass: BypassRules,
    credentials: CredentialCache,
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
        self.proxy_for_url_with(url, load_proxy_credentials)
    }

    fn proxy_for_url_with(
        &self,
        url: &Url,
        load: impl FnOnce(&ProxyEndpoint) -> Option<ProxyCredentials>,
    ) -> Option<String> {
        let resolver = {
            let mut cache = self.cache.lock().ok()?;
            if cache
                .checked_at
                .is_none_or(|checked| checked.elapsed() >= SYSTEM_PROXY_REFRESH_INTERVAL)
            {
                cache.checked_at = Some(Instant::now());
                cache.update(SystemProxyConfiguration::from_settings());
            }
            cache.resolver.clone()
        };
        resolver.and_then(|resolver| resolver.proxy_for_url_with(url, load))
    }
}

impl SystemProxyCache {
    fn update(&mut self, configuration: Option<SystemProxyConfiguration>) {
        self.update_at(configuration, Instant::now());
    }

    fn update_at(&mut self, configuration: Option<SystemProxyConfiguration>, now: Instant) {
        let unchanged = self.configuration == configuration;
        if unchanged && self.reload_retry.is_none_or(|retry| now < retry.at) {
            return;
        }
        let previous_backoff = self
            .reload_retry
            .filter(|_| unchanged)
            .map(|retry| retry.backoff);

        // pacparser keeps one process-global engine. Replacing the resolver reloads
        // that engine under its own mutex while this cache mutex excludes evaluations.
        self.resolver = None;
        self.configuration = configuration.clone();
        let (resolver, reload_failed) = configuration
            .map(SystemProxyResolver::from_configuration)
            .unwrap_or((None, false));
        self.resolver = resolver.map(Arc::new);
        self.reload_retry = reload_failed.then(|| {
            let backoff = previous_backoff.map_or(PAC_RELOAD_INITIAL_BACKOFF, |backoff| {
                (backoff * 2).min(PAC_RELOAD_MAX_BACKOFF)
            });
            PacReloadRetry {
                at: now + backoff,
                backoff,
            }
        });
    }
}

impl SystemProxyConfiguration {
    fn from_settings() -> Option<Self> {
        let store = SCDynamicStoreBuilder::new("OpenQuota").build()?;
        let settings = store.get_proxies()?;
        let fallback = StaticProxyConfig::from_settings(&settings);
        let bypass = BypassRules::from_system_settings(&settings);
        let pac = if flag_setting(&settings, unsafe { kSCPropNetProxiesProxyAutoConfigEnable }) {
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
            credentials: CredentialCache::default(),
        });
        (resolver, reload_failed)
    }

    #[cfg(test)]
    fn proxy_for_url(&self, url: &Url) -> Option<String> {
        self.proxy_for_url_with(url, load_proxy_credentials)
    }

    fn proxy_for_url_with(
        &self,
        url: &Url,
        load: impl FnOnce(&ProxyEndpoint) -> Option<ProxyCredentials>,
    ) -> Option<String> {
        if self.bypass.matches(url) {
            return None;
        }
        let proxy = if let Some(pac) = &self.pac {
            self.apply_pac_decision(url, pac.decision_for_url(url))
        } else {
            self.fallback.as_ref()?.proxy_for_url(url)
        }?;
        Some(self.proxy_with_credentials_with(proxy, load))
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

    #[cfg(test)]
    fn proxy_with_credentials(&self, proxy: String) -> String {
        self.proxy_with_credentials_with(proxy, load_proxy_credentials)
    }

    fn proxy_with_credentials_with(
        &self,
        proxy: String,
        load: impl FnOnce(&ProxyEndpoint) -> Option<ProxyCredentials>,
    ) -> String {
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
        // Look up credentials on the first request to this proxy. Cache misses too,
        // so an unauthenticated proxy only needs one Keychain lookup per configuration.
        let Some(credentials) = self.credentials.get_or_load(endpoint, load) else {
            return proxy;
        };
        if url.set_username(&credentials.username).is_err()
            || url.set_password(Some(&credentials.password)).is_err()
        {
            return proxy;
        }
        url.into()
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

#[cfg(test)]
mod tests {
    use super::{
        pac_decision_from_result, BypassRules, CredentialCache, CredentialEntry,
        DynamicSystemProxyResolver, EnvironmentProxyConfig, HostMatch, PacDecision, PacResolver,
        ProxyCredentials, ProxyEndpoint, ProxyResolver, StaticProxyConfig, SystemProxyCache,
        SystemProxyConfiguration, SystemProxyResolver,
    };
    use reqwest::Url;
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc, Arc, Condvar, Mutex,
        },
        time::{Duration, Instant},
    };

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    fn dynamic_system(resolver: SystemProxyResolver) -> DynamicSystemProxyResolver {
        DynamicSystemProxyResolver {
            cache: Mutex::new(SystemProxyCache {
                checked_at: Some(Instant::now()),
                resolver: Some(Arc::new(resolver)),
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
                credentials: CredentialCache::default(),
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
                    bypass: BypassRules::from_entries(["bypass.example"], false, HostMatch::Exact),
                    http: Some("http://system-http:8080/".into()),
                    https: Some("http://system-https:8443/".into()),
                }),
                bypass: BypassRules::from_entries(["bypass.example"], false, HostMatch::Exact),
                credentials: CredentialCache::default(),
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
    fn failed_pac_reload_backs_off_until_configuration_changes() {
        let mut configuration = static_configuration(Some("http://fallback:8080"));
        configuration.pac = Some(super::PacSource::Url(
            "file:///nonexistent/openquota-test.pac".into(),
        ));
        let start = Instant::now();
        let mut cache = SystemProxyCache::default();

        cache.update_at(Some(configuration.clone()), start);
        let first = cache.reload_retry.expect("failed PAC reload is retried");
        assert_eq!(first.backoff, super::PAC_RELOAD_INITIAL_BACKOFF);
        assert_eq!(
            cache
                .resolver
                .as_ref()
                .and_then(|resolver| resolver.proxy_for_url(&url("https://example.com"))),
            Some("http://fallback:8080".into())
        );

        cache.update_at(Some(configuration.clone()), start + Duration::from_secs(1));
        assert_eq!(cache.reload_retry.unwrap().at, first.at);

        cache.update_at(Some(configuration.clone()), first.at);
        assert_eq!(
            cache.reload_retry.unwrap().backoff,
            super::PAC_RELOAD_INITIAL_BACKOFF * 2
        );

        cache.update_at(
            Some(static_configuration(Some("http://fallback:8080"))),
            first.at,
        );
        assert!(cache.reload_retry.is_none());
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
            credentials: CredentialCache {
                entries: Mutex::new(HashMap::from([(
                    endpoint,
                    CredentialEntry::Loaded(Some(ProxyCredentials {
                        username: zeroize::Zeroizing::new("user@example".into()),
                        password: zeroize::Zeroizing::new("p@ss/word".into()),
                    })),
                )])),
                ready: Condvar::new(),
                waiters: AtomicUsize::new(0),
            },
        };

        assert_eq!(
            resolver.proxy_with_credentials("http://proxy.example:8080".into()),
            "http://user%40example:p%40ss%2Fword@proxy.example:8080/"
        );
    }

    #[test]
    fn dynamic_resolver_releases_cache_and_credentials_locks_during_lookup() {
        let resolver = Arc::new(SystemProxyResolver {
            pac: None,
            fallback: Some(StaticProxyConfig {
                bypass: BypassRules::default(),
                http: Some("http://http-proxy.example:8081".into()),
                https: Some("http://proxy.example:8080".into()),
            }),
            bypass: BypassRules::from_entries(["bypass.example"], false, HostMatch::Exact),
            credentials: CredentialCache::default(),
        });
        let dynamic = DynamicSystemProxyResolver {
            cache: Mutex::new(SystemProxyCache {
                checked_at: Some(Instant::now()),
                configuration: Some(static_configuration(Some("http://proxy.example:8080"))),
                resolver: Some(Arc::clone(&resolver)),
                reload_retry: None,
            }),
        };

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        std::thread::scope(|scope| {
            let dynamic_ref = &dynamic;
            let first = scope.spawn(move || {
                dynamic_ref.proxy_for_url_with(&url("https://target.example"), |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    None
                })
            });

            entered_rx.recv().unwrap();

            assert!(dynamic.cache.try_lock().is_ok());
            assert!(resolver.credentials.entries.try_lock().is_ok());
            assert_eq!(
                dynamic.proxy_for_url_with(&url("https://bypass.example"), |_| unreachable!()),
                None
            );
            assert_eq!(
                dynamic.proxy_for_url_with(&url("http://other.example"), |_| None),
                Some("http://http-proxy.example:8081".into())
            );

            // Updating the cached resolver while Thread 1 is blocked does not deadlock
            // and Thread 1 still completes safely on its cloned Arc.
            dynamic
                .cache
                .lock()
                .unwrap()
                .update(Some(static_configuration(Some(
                    "http://updated.example:8080",
                ))));

            release_tx.send(()).unwrap();
            assert_eq!(
                first.join().unwrap(),
                Some("http://proxy.example:8080".into())
            );
        });

        // Follow-up lookup on the original resolver hits cached `None` without calling `load`.
        assert_eq!(
            resolver.proxy_for_url_with(&url("https://target.example"), |_| unreachable!()),
            Some("http://proxy.example:8080".into())
        );
    }

    #[test]
    fn concurrent_lookups_for_same_endpoint_share_one_inflight_load() {
        let resolver = SystemProxyResolver {
            pac: None,
            fallback: None,
            bypass: BypassRules::default(),
            credentials: CredentialCache::default(),
        };
        let loads = AtomicUsize::new(0);
        let (first_entered_tx, first_entered_rx) = mpsc::channel();
        let (first_release_tx, first_release_rx) = mpsc::channel();

        std::thread::scope(|scope| {
            let resolver_ref = &resolver;
            let loads_ref = &loads;
            let first = scope.spawn(move || {
                resolver_ref.proxy_with_credentials_with(
                    "http://shared-proxy.example:8080".into(),
                    |_| {
                        loads_ref.fetch_add(1, Ordering::SeqCst);
                        first_entered_tx.send(()).unwrap();
                        first_release_rx.recv().unwrap();
                        Some(ProxyCredentials {
                            username: zeroize::Zeroizing::new("user_a".into()),
                            password: zeroize::Zeroizing::new("pass_a".into()),
                        })
                    },
                )
            });

            first_entered_rx.recv().unwrap();

            // A different endpoint proceeds in parallel without waiting for the first lookup.
            let other = resolver
                .proxy_with_credentials_with("http://other-proxy.example:8081".into(), |_| None);
            assert_eq!(other, "http://other-proxy.example:8081");

            // A second lookup for the same endpoint waits for the in-flight load instead of
            // racing a duplicate Keychain query or caching a premature `None`.
            let second = scope.spawn(move || {
                resolver_ref
                    .proxy_with_credentials_with("http://shared-proxy.example:8080".into(), |_| {
                        unreachable!("in-flight lookup should be shared")
                    })
            });

            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            let mut entered_wait = false;
            while Instant::now() < deadline {
                if resolver.credentials.waiters.load(Ordering::SeqCst) > 0 {
                    entered_wait = true;
                    break;
                }
                std::thread::yield_now();
            }

            first_release_tx.send(()).unwrap();
            let first_result = first.join().unwrap();
            let second_result = second.join().unwrap();
            assert!(entered_wait, "second lookup did not enter condvar wait");
            assert_eq!(
                first_result,
                "http://user_a:pass_a@shared-proxy.example:8080/"
            );
            assert_eq!(
                second_result,
                "http://user_a:pass_a@shared-proxy.example:8080/"
            );
        });

        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert_eq!(
            resolver.proxy_with_credentials_with("http://shared-proxy.example:8080".into(), |_| {
                unreachable!()
            }),
            "http://user_a:pass_a@shared-proxy.example:8080/"
        );
    }

    #[test]
    fn a_pac_script_that_never_returns_fails_and_frees_the_engine() {
        let looping = PacResolver::new(
            "function FindProxyForURL(url, host) { for (;;) { try { while (true) {} } catch (e) {} } }",
        )
        .unwrap();
        assert_eq!(
            looping.decision_for_url(&url("https://example.com")),
            PacDecision::Error
        );
        assert!(PacResolver::new("while (true) {}").is_none());

        let working =
            PacResolver::new("function FindProxyForURL(url, host) { return 'DIRECT'; }").unwrap();
        assert_eq!(
            working.decision_for_url(&url("https://example.com")),
            PacDecision::Direct
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
            credentials: CredentialCache::default(),
        };
        let target = url("https://example.com");
        assert_eq!(
            resolver
                .apply_pac_decision(&target, PacDecision::Proxy("http://pac-proxy:8118/".into())),
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
    fn bypass_rules_match_ipv6_literals() {
        let rules = BypassRules::from_comma_list("::1,fd00::/8", false);
        assert!(rules.matches(&url("http://[::1]:8080")));
        assert!(rules.matches(&url("http://[fd00::5]:8080")));
        assert!(!rules.matches(&url("http://[2001:db8::1]")));

        let simple = BypassRules::from_comma_list("<local>", false);
        assert!(!simple.matches(&url("http://[2001:db8::1]")));
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
