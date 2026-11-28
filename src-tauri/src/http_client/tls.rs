/// Return only the certificate diagnostic, not request URLs or credentials.
pub(crate) fn certificate_restart_hint(error: &reqwest::Error) -> Option<String> {
    use std::error::Error;
    let mut source = error.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        if certificate_verification_failure(&message) || certificate_status_failure(cause) {
            return Some(format!("Certificate verification failed. If you updated the system certificates, fully quit and restart OpenQuota, then retry. Otherwise, check the proxy certificate and system trust settings. ({message})"));
        }
        source = cause.source();
    }
    None
}

/// Certificate verification failures can persist in cached clients after CA updates.
fn certificate_verification_failure(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "invalid peer certificate",
        "certificate verify failed",
        "certificate verification failed",
        "unknownissuer",
        "unknown issuer",
        "unable to get local issuer certificate",
        "self-signed certificate",
        "self signed certificate",
        "certificate has expired",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

/// Security framework statuses for a rejected server certificate.
#[cfg(target_os = "macos")]
const CERTIFICATE_STATUS_CODES: &[i32] = &[
    -9807,  // errSSLXCertChainInvalid
    -9808,  // errSSLBadCert
    -9812,  // errSSLUnknownRootCert
    -9813,  // errSSLNoRootCert
    -9814,  // errSSLCertExpired
    -9815,  // errSSLCertNotYetValid
    -9843,  // errSSLHostNameMismatch
    -67602, // errSecHostNameMismatch
    -67808, // errSecVerifyFailed
    -67818, // errSecCertificateExpired
    -67819, // errSecCertificateNotValidYet
    -67843, // errSecNotTrusted
];

/// SChannel and certificate chain HRESULTs for a rejected server certificate.
#[cfg(target_os = "windows")]
const CERTIFICATE_STATUS_CODES: &[i32] = &[
    0x800B_0101_u32 as i32, // CERT_E_EXPIRED
    0x800B_0109_u32 as i32, // CERT_E_UNTRUSTEDROOT
    0x800B_010A_u32 as i32, // CERT_E_CHAINING
    0x800B_010F_u32 as i32, // CERT_E_CN_NO_MATCH
    0x800B_0112_u32 as i32, // CERT_E_UNTRUSTEDCA
    0x8009_6004_u32 as i32, // TRUST_E_CERT_SIGNATURE
    0x8009_0322_u32 as i32, // SEC_E_WRONG_PRINCIPAL
    0x8009_0325_u32 as i32, // SEC_E_UNTRUSTED_ROOT
    0x8009_0327_u32 as i32, // SEC_E_CERT_UNKNOWN
    0x8009_0328_u32 as i32, // SEC_E_CERT_EXPIRED
];

/// The native TLS backends of macOS and Windows describe failures in the system language, so
/// they are recognized by status code instead of by message.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn certificate_status_failure(cause: &(dyn std::error::Error + 'static)) -> bool {
    let Some(error) = cause.downcast_ref::<native_tls::Error>() else {
        return false;
    };
    // native-tls exposes the status only through Debug: `Error { code: -67843, .. }` from
    // security-framework, `Os { code: -2146762487, .. }` from SChannel.
    let debug = format!("{error:?}");
    debug
        .split_once("code: ")
        .and_then(|(_, rest)| {
            let end = rest
                .char_indices()
                .find(|&(index, character)| {
                    !(character.is_ascii_digit() || (index == 0 && character == '-'))
                })
                .map_or(rest.len(), |(index, _)| index);
            rest[..end].parse::<i32>().ok()
        })
        .is_some_and(|code| CERTIFICATE_STATUS_CODES.contains(&code))
}

/// OpenSSL and rustls describe failures in English, which the message check covers.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn certificate_status_failure(_cause: &(dyn std::error::Error + 'static)) -> bool {
    false
}

#[cfg(test)]
mod tls_tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::{Duration, SystemTime},
    };

    use super::super::{blocking_client_builder, client_builder_for_proxy, DIRECT_PROXY};
    use super::certificate_restart_hint;

    const KEY: &[u8] = include_bytes!("../../tests/fixtures/tls/localhost-key.pem");

    fn request(
        trusted: bool,
        host: &str,
        direct: bool,
        expired: bool,
    ) -> Result<String, reqwest::Error> {
        let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::DigitalSignature,
            rcgen::KeyUsagePurpose::KeyEncipherment,
            rcgen::KeyUsagePurpose::KeyCertSign,
        ];
        // macOS limits server-certificate lifetimes even for roots supplied by
        // a client. Generate a short-lived certificate so this fixture never ages out.
        let now = SystemTime::now();
        let day = Duration::from_secs(86400);
        params.not_before = (now - day * 2).into();
        params.not_after = if expired { now - day } else { now + day }.into();
        let key = rcgen::KeyPair::from_pem(std::str::from_utf8(KEY).unwrap()).unwrap();
        let cert = params.self_signed(&key).unwrap().pem();
        let identity = native_tls::Identity::from_pkcs8(cert.as_bytes(), KEY).unwrap();
        let acceptor = native_tls::TlsAcceptor::new(identity).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            if let Ok(mut stream) = acceptor.accept(socket) {
                let mut buffer = [0; 4096];
                if stream.read(&mut buffer).is_ok_and(|size| size > 0) {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ntrusted",
                    );
                }
            }
        });
        let mut builder = if direct {
            client_builder_for_proxy(Some(DIRECT_PROXY)).unwrap()
        } else {
            blocking_client_builder().no_proxy()
        };
        if trusted {
            builder =
                builder.tls_certs_merge([reqwest::Certificate::from_pem(cert.as_bytes()).unwrap()]);
        }
        let result = builder
            .resolve("localhost", address)
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("https://{host}:{}/", address.port()))
            .send()
            .and_then(|response| response.text());
        server.join().unwrap();
        result
    }

    #[test]
    fn trusted_self_signed_ca_server_works_with_native_tls() {
        for direct in [false, true] {
            assert_eq!(
                request(true, "localhost", direct, false).unwrap(),
                "trusted"
            );
        }
    }

    #[test]
    fn certificate_hint_survives_a_real_tls_error_chain() {
        // Untrusted, wrong host name, and expired.
        for (trusted, host, expired) in [
            (false, "localhost", false),
            (true, "127.0.0.1", false),
            (true, "localhost", true),
        ] {
            let error = request(trusted, host, true, expired).unwrap_err();
            let hint = certificate_restart_hint(&error)
                .unwrap_or_else(|| panic!("certificate restart hint for {error:?}"));
            assert!(hint.contains("fully quit and restart OpenQuota"));
            assert!(!hint.contains("https://"));
        }
    }

    #[test]
    fn untrusted_self_signed_server_is_rejected() {
        assert!(request(false, "localhost", true, false)
            .unwrap_err()
            .is_connect());
    }

    #[test]
    fn trusted_server_with_wrong_hostname_is_rejected() {
        assert!(request(true, "127.0.0.1", true, false)
            .unwrap_err()
            .is_connect());
    }

    #[test]
    fn expired_trusted_server_is_rejected() {
        assert!(request(true, "localhost", true, true)
            .unwrap_err()
            .is_connect());
    }
}

#[cfg(test)]
mod certificate_hint_tests {
    use super::certificate_verification_failure;

    #[test]
    fn recognizes_verification_errors_without_labeling_other_network_failures() {
        for message in [
            "invalid peer certificate: BadSignature",
            "certificate verify failed: unable to get local issuer certificate",
            "invalid peer certificate: UnknownIssuer",
        ] {
            assert!(certificate_verification_failure(message));
        }
        for message in [
            "connection refused",
            "operation timed out",
            "HTTP 401",
            "TLS handshake EOF",
        ] {
            assert!(!certificate_verification_failure(message));
        }
    }
}
