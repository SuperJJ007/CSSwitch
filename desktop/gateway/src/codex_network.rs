use std::fmt;

#[cfg(test)]
use csswitch_codex_network::direct_route;
use csswitch_codex_network::{route_from_process_env, ResolvedCodexNetworkRoute};

#[derive(Clone)]
pub(crate) struct CodexHttpClientFactory {
    route: ResolvedCodexNetworkRoute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CodexNetworkError {
    pub(crate) code: &'static str,
    detail: &'static str,
}

impl fmt::Display for CodexNetworkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.detail)
    }
}

impl std::error::Error for CodexNetworkError {}

impl fmt::Debug for CodexHttpClientFactory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexHttpClientFactory")
            .field("source", &self.route.source)
            .field("proxy_scheme", &self.route.proxy_scheme)
            .field("fingerprint", &self.route.fingerprint)
            .finish()
    }
}

impl CodexHttpClientFactory {
    pub(crate) fn from_environment() -> Result<Self, CodexNetworkError> {
        let route = route_from_process_env().map_err(|error| CodexNetworkError {
            code: error.code(),
            detail: "Codex 网络路由配置非法",
        })?;
        Ok(Self { route })
    }

    #[cfg(test)]
    pub(crate) fn direct_for_test() -> Self {
        Self {
            route: direct_route(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_route(route: ResolvedCodexNetworkRoute) -> Self {
        Self { route }
    }

    pub(crate) fn async_builder(&self) -> Result<reqwest::ClientBuilder, CodexNetworkError> {
        let mut builder = reqwest::Client::builder().no_proxy();
        if let Some(proxy) = self.proxy()? {
            builder = builder.proxy(proxy);
        }
        Ok(builder)
    }

    pub(crate) fn blocking_builder(
        &self,
    ) -> Result<reqwest::blocking::ClientBuilder, CodexNetworkError> {
        let mut builder = reqwest::blocking::Client::builder().no_proxy();
        if let Some(proxy) = self.proxy()? {
            builder = builder.proxy(proxy);
        }
        Ok(builder)
    }

    #[cfg(test)]
    pub(crate) fn route(&self) -> &ResolvedCodexNetworkRoute {
        &self.route
    }

    pub(crate) fn has_proxy(&self) -> bool {
        self.route.proxy_url.is_some()
    }

    fn proxy(&self) -> Result<Option<reqwest::Proxy>, CodexNetworkError> {
        let Some(proxy_url) = self.route.proxy_url.as_deref() else {
            return Ok(None);
        };
        let mut proxy = reqwest::Proxy::all(proxy_url).map_err(|_| CodexNetworkError {
            code: "proxy_config_invalid",
            detail: "Codex 代理无法应用",
        })?;
        if let Some(no_proxy) = self.route.no_proxy.as_deref() {
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(no_proxy));
        }
        Ok(Some(proxy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use csswitch_codex_network::{
        resolve, CodexNetworkMode, CodexNetworkSettings, EnvironmentSnapshot,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;

    const CERT_DER_B64: &str = "MIIC2zCCAcOgAwIBAgIJAIKzSBO+SyAbMA0GCSqGSIb3DQEBCwUAMBwxGjAYBgNVBAMMEXVucmVzb2x2YWJsZS50ZXN0MB4XDTI2MDcxNjE0MDIwMVoXDTM2MDcxMzE0MDIwMVowHDEaMBgGA1UEAwwRdW5yZXNvbHZhYmxlLnRlc3QwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQC3UzJsaETnJQbdsoN2XlId+tlSiXOnx0xQOhfHKuKoRdAo/qv29s3bHbU5X+S222852jWH982c9Vjtpv6GJyK/RgcKuCgm3JPyJbQkzUdHrgtX16zJ9QACEn4OyU/YzpIGxHZdM+D0tqk48x9JB8/4Arap4BqveyznXuXkyPnte0OMYe8nGS2cMc+U+nj+0PFUthTHkFZr7F5bWJ8pKXr1G7NKurqB921szy5eoCNhC9pW3WwqETI7MPTBRz+S565YXyLhkjB5qvSqlngeShc/UgloTPslJw79M84tU9v6saRXYQzKhkwQokfji/m9oiur/mW7bPxbrnuT7MTsAxT1AgMBAAGjIDAeMBwGA1UdEQQVMBOCEXVucmVzb2x2YWJsZS50ZXN0MA0GCSqGSIb3DQEBCwUAA4IBAQBY+6PgXLyL/9qBSFkx80VXHq8lo0NKZeHx7OIhGz9ucTAYuurXHyfMCXGycDudwq3Z8WuuFrfzQ1xZEdYPyK/eqc1PI3bq5BU+kelnH05nxu8BYRxNoViMWxAKw52gu3Bts+tA8Hrd9OqMcF8MHKEu3fI6BS2xwe6rQmSlcTe934oYjNeAiC9/+fltOx0Vw6z4nW2mmQ2E373jeLBaFVDcTdSJOgNSo6662xksILeGpoCXdg4mjUzFC6VNFmAVicxx67s9YCOWpWIWmP6pGeTCLSFIGk1tzp5UOzKkN9w/AF0dEcil2ZsvJrmyaSsaBP+Wetxm02nTl64maWoASNAW";
    const KEY_DER_B64: &str = "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC3UzJsaETnJQbdsoN2XlId+tlSiXOnx0xQOhfHKuKoRdAo/qv29s3bHbU5X+S222852jWH982c9Vjtpv6GJyK/RgcKuCgm3JPyJbQkzUdHrgtX16zJ9QACEn4OyU/YzpIGxHZdM+D0tqk48x9JB8/4Arap4BqveyznXuXkyPnte0OMYe8nGS2cMc+U+nj+0PFUthTHkFZr7F5bWJ8pKXr1G7NKurqB921szy5eoCNhC9pW3WwqETI7MPTBRz+S565YXyLhkjB5qvSqlngeShc/UgloTPslJw79M84tU9v6saRXYQzKhkwQokfji/m9oiur/mW7bPxbrnuT7MTsAxT1AgMBAAECggEAK+2rr35sxFaDBqy4A60mUDjDyptVM2b2SmMhP4BvP5M7BhfAbTVGrrK3sj/gNlDunhZDrYkbo/jGjmvtoYfPM7Y8Cb2HIYjJisSuHgNyiSKTZUExDlO+5MA5pKFomLMnGqgJFNxRk1IRyqu3W3CbzPoZeytQOaxyXh7HR8NA4D65VpcKF268i/pbjjgOJEgUWWEJIH2FQXFbpK+WQ9Ekz1zcBFMV+28NE0d9NInbTbLHYSPsEViP5ZZ0C2xWlLU28Xh97ZjDCC8U2XyFzhhh8ZVn8xshspm/hw1pJKF8tM2+i9RBrATvP6twkvcP/dSVeYLvm8jHgG+ZcdlT7G2LAQKBgQDgPVZGfqCSn7YcFqOKb7Kxm5eSAFWUneQqAPQRdftbg0ayYHVXvanTULSVzbVrgOGcy4gsR+m7EKxUcUJ1GmI1S/dtn1xhHhY+Kujh6UWPomlkvQ+jQR1hHlLg76b7lTX7w4Ivip88AQr8kceiRfJrEK1/MZOqwauRNPzVZHCguQKBgQDRSlgzLmQyOH2qOY2iU881ZZE7rbDRNdz0e5ho3jnZiQSt9BENgEaui+xsB5uPdHm1lyWQZH3HRQTLlDfhRqRSNPj/XToadcJn2cVVWW6vW1JiBQsvwrn2XfNPzLHw2zSo0BNUs/4XriEBiCeq+3WI0jaJ92OiC+BTVFQg/hfgHQKBgQCqFvyNRlmoPksVbTqptGY4AExtK6G+tDEwhz6azAJYfPAwN6hqYGwj5NDF3J5jKAR6OYxWAkpRYalF+A8v4k5iHPhWh428AOVgTI4PZjEkbU5CYoItFCQj2auGAWKI7Lpg+QCT7TMxgZ0CzdU+yo3CFolztHhNCtCHuUia2K/xyQKBgD9E3kz6pUeZVEP1ih+cfnOB9Nm5tE5KnjU6d+Sb6ZkdltCPi+gs8zEpE5vE4P4JFBIVU0HHX06ySrTQZeQwWtSPNwbbxAjjuJV0e/dFRfS1Ar6nD66si1MzK67gDprlaZHu9SkSEKpP9aJk6rkBs5JdGiezJeeC95m5UIV4yvbxAoGBAIY6ltTtYXpOLZ0/2TfhP451gpVNyL9TIQWuYXicqL7askjkLgOB3alnRLZ+j7wPL+NYlTCTP4uTf4GgAmkmqOBe+5OcSVNiBxAjfbIC5q6EECnKMfgJxIO/LxmQ+7YsCjE99Np3NGEfTLY4cA2e2jQn/hmXeFVy1IZBeFfsR8lX";

    fn test_route(scheme: &str, port: u16) -> ResolvedCodexNetworkRoute {
        resolve(
            &CodexNetworkSettings {
                mode: CodexNetworkMode::Custom,
                proxy_url: format!("{scheme}://127.0.0.1:{port}"),
                ..Default::default()
            },
            &EnvironmentSnapshot::default(),
        )
        .unwrap()
    }

    fn spawn_tls_upstream() -> (SocketAddr, thread::JoinHandle<()>) {
        let cert = base64::engine::general_purpose::STANDARD
            .decode(CERT_DER_B64)
            .unwrap();
        let key = base64::engine::general_purpose::STANDARD
            .decode(KEY_DER_B64)
            .unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
            )
            .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut tls = rustls::StreamOwned::new(connection, stream);
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let read = tls.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            assert!(String::from_utf8_lossy(&request).starts_with("GET /route HTTP/1.1"));
            tls.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
            )
            .unwrap();
            tls.flush().unwrap();
        });
        (address, handle)
    }

    fn relay(mut client: TcpStream, mut upstream: TcpStream) {
        let mut client_read = client.try_clone().unwrap();
        let mut upstream_write = upstream.try_clone().unwrap();
        let forward = thread::spawn(move || {
            let _ = std::io::copy(&mut client_read, &mut upstream_write);
            let _ = upstream_write.shutdown(Shutdown::Write);
        });
        let _ = std::io::copy(&mut upstream, &mut client);
        let _ = client.shutdown(Shutdown::Write);
        forward.join().unwrap();
    }

    fn spawn_connect_proxy(upstream: SocketAddr) -> (SocketAddr, thread::JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                client.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            let request_line = String::from_utf8_lossy(&head)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            let upstream = TcpStream::connect(upstream).unwrap();
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .unwrap();
            client.flush().unwrap();
            relay(client, upstream);
            request_line
        });
        (address, handle)
    }

    fn spawn_socks5h_proxy(
        upstream: SocketAddr,
    ) -> (SocketAddr, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            let mut hello = [0_u8; 2];
            client.read_exact(&mut hello).unwrap();
            assert_eq!(hello[0], 5);
            let mut methods = vec![0_u8; hello[1] as usize];
            client.read_exact(&mut methods).unwrap();
            assert!(methods.contains(&0));
            client.write_all(&[5, 0]).unwrap();
            let mut request = [0_u8; 4];
            client.read_exact(&mut request).unwrap();
            assert_eq!(&request[..3], &[5, 1, 0]);
            assert_eq!(request[3], 3, "SOCKS5h must send a domain address");
            let mut length = [0_u8; 1];
            client.read_exact(&mut length).unwrap();
            let mut domain = vec![0_u8; length[0] as usize];
            client.read_exact(&mut domain).unwrap();
            let mut port = [0_u8; 2];
            client.read_exact(&mut port).unwrap();
            tx.send(String::from_utf8(domain).unwrap()).unwrap();
            let upstream = TcpStream::connect(upstream).unwrap();
            let reply = [
                5,
                0,
                0,
                1,
                127,
                0,
                0,
                1,
                (upstream.peer_addr().unwrap().port() >> 8) as u8,
                upstream.peer_addr().unwrap().port() as u8,
            ];
            client.write_all(&reply).unwrap();
            client.flush().unwrap();
            relay(client, upstream);
        });
        (address, rx, handle)
    }

    fn routed_client(route: ResolvedCodexNetworkRoute) -> reqwest::Client {
        let cert = base64::engine::general_purpose::STANDARD
            .decode(CERT_DER_B64)
            .unwrap();
        CodexHttpClientFactory::for_test_route(route)
            .async_builder()
            .unwrap()
            .add_root_certificate(reqwest::Certificate::from_der(&cert).unwrap())
            .build()
            .unwrap()
    }

    #[test]
    fn direct_factory_builds_both_client_kinds() {
        let factory = CodexHttpClientFactory::direct_for_test();
        assert_eq!(factory.route().source.as_str(), "direct");
        factory.async_builder().unwrap().build().unwrap();
        factory.blocking_builder().unwrap().build().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_proxy_fixture_performs_real_connect_and_tls() {
        let (upstream, upstream_thread) = spawn_tls_upstream();
        let (proxy, proxy_thread) = spawn_connect_proxy(upstream);
        let response = routed_client(test_route("http", proxy.port()))
            .get(format!(
                "https://unresolvable.test:{}/route",
                upstream.port()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        let request_line = proxy_thread.join().unwrap();
        assert_eq!(
            request_line,
            format!("CONNECT unresolvable.test:{} HTTP/1.1", upstream.port())
        );
        upstream_thread.join().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn socks5h_fixture_resolves_unresolvable_domain_at_proxy() {
        let (upstream, upstream_thread) = spawn_tls_upstream();
        let (proxy, domain, proxy_thread) = spawn_socks5h_proxy(upstream);
        let response = routed_client(test_route("socks5h", proxy.port()))
            .get(format!(
                "https://unresolvable.test:{}/route",
                upstream.port()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        assert_eq!(domain.recv().unwrap(), "unresolvable.test");
        proxy_thread.join().unwrap();
        upstream_thread.join().unwrap();
    }
}
