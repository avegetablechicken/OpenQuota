use reqwest::blocking::ClientBuilder;

pub(in crate::http_client) fn configure(builder: ClientBuilder) -> ClientBuilder {
    builder
}

pub(in crate::http_client) fn configure_environment(
    builder: ClientBuilder,
    read: impl Fn(&[&str]) -> Option<String>,
) -> ClientBuilder {
    let bypass =
        read(&["NO_PROXY", "no_proxy"]).and_then(|value| reqwest::NoProxy::from_string(&value));
    let http_names: &[&str] = if std::env::var_os("REQUEST_METHOD").is_some() {
        &["http_proxy"]
    } else {
        &["HTTP_PROXY", "http_proxy"]
    };
    let http = read(http_names);
    let https = read(&["HTTPS_PROXY", "https_proxy"]);
    let all = read(&["ALL_PROXY", "all_proxy"]);
    if http.is_none() && https.is_none() && all.is_none() {
        return builder;
    }
    let mut builder = builder.no_proxy();
    for (scheme, value) in [("http", http), ("https", https), ("all", all)] {
        let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
            continue;
        };
        let proxy = match scheme {
            "http" => reqwest::Proxy::http(value),
            "https" => reqwest::Proxy::https(value),
            _ => reqwest::Proxy::all(value),
        };
        if let Ok(proxy) = proxy {
            builder = builder.proxy(proxy.no_proxy(bypass.clone()));
        }
    }
    builder
}
