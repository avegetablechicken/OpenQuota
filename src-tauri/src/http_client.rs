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
        os::raw::{c_char, c_int},
        sync::{Mutex, OnceLock},
        time::Duration,
    };

    use reqwest::{blocking::ClientBuilder, Proxy, Url};
    use system_configuration::{
        core_foundation::{
            base::CFType,
            number::CFNumber,
            string::{CFString, CFStringRef},
        },
        dynamic_store::SCDynamicStoreBuilder,
        sys::schema_definitions::{
            kSCPropNetProxiesProxyAutoConfigEnable, kSCPropNetProxiesProxyAutoConfigJavaScript,
            kSCPropNetProxiesProxyAutoConfigURLString,
        },
    };

    const MAX_PAC_BYTES: u64 = 4 * 1024 * 1024;
    const PAC_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
    static PAC_RESOLVER: OnceLock<Option<PacResolver>> = OnceLock::new();

    unsafe extern "C" {
        fn pacparser_init() -> c_int;
        fn pacparser_parse_pac_string(script: *const c_char) -> c_int;
        fn pacparser_find_proxy(url: *const c_char, host: *const c_char) -> *const c_char;
        fn pacparser_cleanup();
    }

    pub(super) fn configure(builder: ClientBuilder) -> ClientBuilder {
        if explicit_proxy_environment_present() {
            return builder;
        }
        let Some(resolver) = PAC_RESOLVER.get_or_init(initialize_system_pac).as_ref() else {
            return builder;
        };
        builder.proxy(Proxy::custom(move |url| resolver.proxy_for_url(url)))
    }

    fn explicit_proxy_environment_present() -> bool {
        [
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
    }

    fn initialize_system_pac() -> Option<PacResolver> {
        let script = match system_pac_script() {
            Ok(Some(script)) => script,
            Ok(None) => return None,
            Err(message) => {
                crate::app_warn!("http", "macOS PAC proxy unavailable: {message}");
                return None;
            }
        };
        match PacResolver::new(&script) {
            Some(resolver) => {
                crate::app_info!("http", "macOS PAC proxy enabled");
                Some(resolver)
            }
            None => {
                crate::app_warn!("http", "macOS PAC script could not be parsed");
                None
            }
        }
    }

    fn system_pac_script() -> Result<Option<String>, &'static str> {
        let Some(store) = SCDynamicStoreBuilder::new("OpenQuota").build() else {
            return Err("system proxy settings could not be read");
        };
        let Some(settings) = store.get_proxies() else {
            return Ok(None);
        };
        let enabled = settings
            .find(unsafe { kSCPropNetProxiesProxyAutoConfigEnable })
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|value| value.to_i32())
            .unwrap_or(0)
            == 1;
        if !enabled {
            return Ok(None);
        }
        if let Some(script) = string_setting(&settings, unsafe {
            kSCPropNetProxiesProxyAutoConfigJavaScript
        }) {
            return Ok(Some(script));
        }
        let Some(url) = string_setting(&settings, unsafe {
            kSCPropNetProxiesProxyAutoConfigURLString
        }) else {
            return Err("automatic proxy configuration has no script or URL");
        };
        load_pac_url(&url).map(Some)
    }

    fn string_setting(
        settings: &system_configuration::core_foundation::dictionary::CFDictionary<
            CFString,
            CFType,
        >,
        key: CFStringRef,
    ) -> Option<String> {
        settings
            .find(key)
            .and_then(|value| value.downcast::<CFString>())
            .map(|value| value.to_string())
            .filter(|value| !value.trim().is_empty())
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
        use super::{initialize_system_pac, proxy_url_from_pac};
        use reqwest::Url;

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
            let resolver = initialize_system_pac().expect("macOS PAC should initialize");
            let url = Url::parse("https://chatgpt.com/backend-api/wham/usage").unwrap();
            assert!(resolver.proxy_for_url(&url).is_some());
        }
    }
}
