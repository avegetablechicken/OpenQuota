use std::{
    collections::HashMap,
    ffi::{CStr, CString},
    io::Read,
    os::raw::{c_char, c_int},
    sync::{Mutex, MutexGuard, OnceLock},
    time::Duration,
};

use reqwest::Url;
use system_configuration::sys::schema_definitions::{
    kSCPropNetProxiesProxyAutoConfigJavaScript, kSCPropNetProxiesProxyAutoConfigURLString,
};

use super::{string_setting, ProxySettings};

const MAX_PAC_BYTES: u64 = 4 * 1024 * 1024;
const PAC_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
static PAC_ENGINE: OnceLock<Mutex<()>> = OnceLock::new();

unsafe extern "C" {
    fn pacparser_init() -> c_int;
    fn pacparser_parse_pac_string(script: *const c_char) -> c_int;
    fn pacparser_find_proxy(url: *const c_char, host: *const c_char) -> *const c_char;
    fn pacparser_cleanup();
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum PacSource {
    Script(String),
    Url(String),
}

impl PacSource {
    pub(super) fn load(self) -> Result<String, &'static str> {
        match self {
            Self::Script(script) => Ok(script),
            Self::Url(url) => std::thread::spawn(move || load_pac_url(&url))
                .join()
                .map_err(|_| "automatic proxy downloader stopped unexpectedly")?,
        }
    }
}

pub(super) fn pac_source(settings: &ProxySettings) -> Option<PacSource> {
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

fn load_pac_url(value: &str) -> Result<String, &'static str> {
    let url = Url::parse(value).map_err(|_| "automatic proxy URL is invalid")?;
    match url.scheme() {
        "http" | "https" => load_http_pac(url),
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|_| "automatic proxy file URL is invalid")?;
            let metadata =
                std::fs::metadata(&path).map_err(|_| "automatic proxy file could not be read")?;
            if metadata.len() > MAX_PAC_BYTES {
                return Err("automatic proxy script is too large");
            }
            std::fs::read_to_string(path).map_err(|_| "automatic proxy file is not valid UTF-8")
        }
        _ => Err("automatic proxy URL scheme is unsupported"),
    }
}

fn load_http_pac(url: Url) -> Result<String, &'static str> {
    let client = crate::http_client::tls_client_builder()
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
pub(super) enum PacDecision {
    Proxy(String),
    Direct,
    Error,
}

pub(super) struct PacResolver {
    script: CString,
    decisions: Mutex<HashMap<String, PacDecision>>,
}

struct PacParserSession {
    _guard: MutexGuard<'static, ()>,
}

impl PacResolver {
    pub(super) fn new(script: &str) -> Option<Self> {
        let script = CString::new(script).ok()?;
        PacParserSession::new(&script)?;
        Some(Self {
            script,
            decisions: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn decision_for_url(&self, url: &Url) -> PacDecision {
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

pub(super) fn pac_decision_from_result(result: &str) -> PacDecision {
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
