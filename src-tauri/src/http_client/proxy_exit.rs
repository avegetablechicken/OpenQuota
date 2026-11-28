use reqwest::blocking::Client;

use super::{client_builder_for_proxy, validate_provider_proxy};

#[derive(Debug, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyExitLocation {
    pub ip: String,
    pub country_code: String,
}

pub(super) const PROXY_LOCATION_ERROR: &str = "The proxy exit location could not be detected.";

pub fn proxy_exit_location(proxy: &str) -> Result<ProxyExitLocation, String> {
    validate_provider_proxy(proxy)?;
    let client = client_builder_for_proxy(Some(proxy))
        .and_then(|builder| {
            builder
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("OpenQuota/", env!("CARGO_PKG_VERSION")))
                .build()
        })
        .map_err(|_| PROXY_LOCATION_ERROR.to_owned())?;
    // Only the fixed diagnostic endpoint is contacted; no provider credentials are attached.
    query_proxy_exit(&client, "https://www.cloudflare.com/cdn-cgi/trace")
}

pub(super) fn query_proxy_exit(
    client: &Client,
    endpoint: &str,
) -> Result<ProxyExitLocation, String> {
    use std::io::Read;
    let response = client
        .get(endpoint)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|_| PROXY_LOCATION_ERROR.to_owned())?;
    if !response.status().is_success() {
        return Err(PROXY_LOCATION_ERROR.to_owned());
    }
    let mut body = String::new();
    response
        .take(4097)
        .read_to_string(&mut body)
        .map_err(|_| PROXY_LOCATION_ERROR.to_owned())?;
    if body.len() > 4096 {
        return Err(PROXY_LOCATION_ERROR.to_owned());
    }
    parse_proxy_exit(&body).ok_or_else(|| PROXY_LOCATION_ERROR.to_owned())
}

pub(super) fn parse_proxy_exit(body: &str) -> Option<ProxyExitLocation> {
    let mut ip = None;
    let mut country_code = None;
    for line in body.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "ip" => {
                ip = value
                    .parse::<std::net::IpAddr>()
                    .ok()
                    .map(|ip| ip.to_string())
            }
            "loc"
                if value.len() == 2
                    && value.bytes().all(|byte| byte.is_ascii_uppercase())
                    && !matches!(value, "XX" | "ZZ") =>
            {
                country_code = Some(value.to_owned())
            }
            _ => {}
        }
    }
    Some(ProxyExitLocation {
        ip: ip?,
        country_code: country_code?,
    })
}
