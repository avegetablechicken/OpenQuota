mod platform;
mod proxy_exit;
mod tls;

#[cfg(test)]
mod provider_proxy_tests;

pub use proxy_exit::{proxy_exit_location, ProxyExitLocation};
pub(crate) use tls::certificate_restart_hint;

use reqwest::blocking::{Client, ClientBuilder};

pub fn blocking_client_builder() -> ClientBuilder {
    platform::configure(tls_client_builder())
}

fn tls_client_builder() -> ClientBuilder {
    // Select explicitly: other dependencies can also enable reqwest's Rustls feature.
    // Use the native trust verifier (Security Framework / SChannel / OpenSSL),
    // including locally trusted self-signed server certificates with CA:TRUE.
    // Providers opt in to compression where supported by their API.
    Client::builder().tls_backend_native().gzip(false)
}

/// Persisted override: absent means system routing, `direct` disables every proxy,
/// and any other value is a validated proxy URL.
pub const DIRECT_PROXY: &str = "direct";

pub fn validate_provider_proxy(value: &str) -> Result<(), String> {
    if value == DIRECT_PROXY {
        Ok(())
    } else {
        validate_proxy_url(value)
    }
}

// Each client retains its provider ID, including independently configured account slots.
static PROVIDER_PROXIES: std::sync::RwLock<std::collections::BTreeMap<String, String>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

pub fn set_provider_proxies(proxies: &std::collections::BTreeMap<String, String>) {
    *PROVIDER_PROXIES
        .write()
        .unwrap_or_else(|error| error.into_inner()) = proxies.clone();
}

pub fn validate_proxy_url(value: &str) -> Result<(), String> {
    let invalid = || {
        "Enter a valid HTTP, HTTPS, SOCKS5 or SOCKS5H proxy URL without a path, query or fragment."
            .to_owned()
    };
    let url = reqwest::Url::parse(value).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
        || url.host_str().is_none()
        || url.port() == Some(0)
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    reqwest::Proxy::all(value).map_err(|_| invalid())?;
    Ok(())
}

fn client_builder_for_proxy(proxy: Option<&str>) -> Result<ClientBuilder, reqwest::Error> {
    Ok(match proxy {
        Some(DIRECT_PROXY) => tls_client_builder().no_proxy(),
        Some(url) => tls_client_builder().no_proxy().proxy(
            reqwest::Proxy::all(url)?
                .no_proxy(reqwest::NoProxy::from_string("localhost,127.0.0.0/8,::1")),
        ),
        None => blocking_client_builder(),
    })
}

type ClientFactory = dyn Fn(ClientBuilder) -> ClientBuilder + Send + Sync;
type CachedClient = Option<(Option<String>, Client)>;

#[derive(Clone)]
pub struct ProviderClient {
    provider_id: String,
    claude_settings: Option<std::path::PathBuf>,
    configure: std::sync::Arc<ClientFactory>,
    cached: std::sync::Arc<std::sync::Mutex<CachedClient>>,
}

impl ProviderClient {
    pub fn new(
        provider_id: &str,
        configure: impl Fn(ClientBuilder) -> ClientBuilder + Send + Sync + 'static,
    ) -> Result<Self, reqwest::Error> {
        let client = Self {
            provider_id: provider_id.to_owned(),
            claude_settings: (provider_id == "claude")
                .then(crate::providers::claude::config::settings_path),
            configure: std::sync::Arc::new(configure),
            cached: Default::default(),
        };
        client.current()?;
        Ok(client)
    }

    pub fn for_provider(&self, provider_id: &str) -> Self {
        Self {
            provider_id: provider_id.to_owned(),
            claude_settings: self.claude_settings.clone(),
            configure: self.configure.clone(),
            cached: Default::default(),
        }
    }

    pub(crate) fn with_claude_settings(mut self, path: std::path::PathBuf) -> Self {
        self.claude_settings = Some(path);
        self.cached = Default::default();
        self
    }

    pub fn current(&self) -> Result<Client, reqwest::Error> {
        let proxy = PROVIDER_PROXIES
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(&self.provider_id)
            .cloned();
        self.current_with_proxy(proxy)
    }

    fn current_with_proxy(&self, proxy: Option<String>) -> Result<Client, reqwest::Error> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((previous, client)) = cached.as_ref() {
            if previous == &proxy {
                return Ok(client.clone());
            }
        }
        let builder = if self.provider_id == "codex" && proxy.is_none() {
            platform::configure_environment(
                tls_client_builder(),
                crate::codex_environment::proxy_value,
            )
        } else if let Some(path) = self
            .claude_settings
            .as_ref()
            .filter(|_| proxy.is_none() && self.provider_id == "claude")
        {
            platform::configure_environment(tls_client_builder(), |names| {
                crate::providers::claude::config::proxy_value(path, names)
            })
        } else {
            client_builder_for_proxy(proxy.as_deref())?
        };
        let client = (self.configure)(builder).build()?;
        *cached = Some((proxy, client.clone()));
        Ok(client)
    }
}
