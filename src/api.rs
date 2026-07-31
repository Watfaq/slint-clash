// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{collections::BTreeMap, time::Duration};

use reqwest::{Method, RequestBuilder};
use serde::Deserialize;

const MAX_STREAM_BUFFER: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct ClashApi {
    client: reqwest::Client,
    base_url: String,
    secret: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct Version {
    pub meta: Option<bool>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct Traffic {
    pub up: u64,
    pub down: u64,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub mode: String,
    #[serde(rename = "mixed-port")]
    pub mixed_port: Option<u16>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProxySnapshot {
    #[serde(default)]
    pub proxies: BTreeMap<String, ProxyEntry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProxyEntry {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub now: String,
    #[serde(default)]
    pub all: Vec<String>,
    #[serde(default)]
    pub alive: bool,
    #[serde(default)]
    pub history: Vec<DelayHistory>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DelayHistory {
    #[serde(default)]
    pub delay: u32,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct ProxyDelay {
    pub delay: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ConfigReloadRequest<'a> {
    path: &'a str,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ModePatchRequest<'a> {
    mode: &'a str,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ProxySelectionRequest<'a> {
    name: &'a str,
}

impl ClashApi {
    pub fn new(base_url: String, secret: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .build()
                .unwrap_or_default(),
            base_url: base_url.trim_end_matches('/').to_owned(),
            secret: secret.filter(|value| !value.is_empty()),
        }
    }

    fn build_request(&self, method: Method, path: &str) -> RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        let mut request = self.client.request(method, url);
        if let Some(secret) = &self.secret {
            request = request.bearer_auth(secret);
        }
        request
    }

    pub async fn version(&self) -> eyre::Result<Version> {
        let response = self
            .build_request(Method::GET, "/version")
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?;
        Ok(response.json().await?)
    }

    pub async fn reload_config(&self, path: &str) -> eyre::Result<()> {
        let response = self
            .build_request(Method::PUT, "/configs")
            .timeout(Duration::from_secs(10))
            .json(&ConfigReloadRequest { path })
            .send()
            .await?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Err(eyre::eyre!(
            "Mihomo rejected the configuration ({status}): {}",
            text.trim()
        ))
    }

    pub async fn runtime_config(&self) -> eyre::Result<RuntimeConfig> {
        let response = self
            .build_request(Method::GET, "/configs")
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?;
        Ok(response.json().await?)
    }

    pub async fn set_mode(&self, mode: &str) -> eyre::Result<()> {
        if !matches!(mode, "rule" | "global" | "direct") {
            return Err(eyre::eyre!("Unsupported Mihomo mode: {mode}"));
        }
        self.build_request(Method::PATCH, "/configs")
            .timeout(Duration::from_secs(5))
            .json(&ModePatchRequest { mode })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn proxies(&self) -> eyre::Result<ProxySnapshot> {
        let response = self
            .build_request(Method::GET, "/proxies")
            .timeout(Duration::from_secs(10))
            .send()
            .await?
            .error_for_status()?;
        Ok(response.json().await?)
    }

    pub async fn select_proxy(&self, group: &str, proxy: &str) -> eyre::Result<()> {
        let path = format!("/proxies/{}", encode_path_segment(group));
        self.build_request(Method::PUT, &path)
            .timeout(Duration::from_secs(5))
            .json(&ProxySelectionRequest { name: proxy })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn proxy_delay(
        &self,
        proxy: &str,
        test_url: &str,
        timeout_ms: u32,
    ) -> eyre::Result<ProxyDelay> {
        let path = format!("/proxies/{}/delay", encode_path_segment(proxy));
        let response = self
            .build_request(Method::GET, &path)
            .query(&[("url", test_url), ("timeout", &timeout_ms.to_string())])
            .timeout(Duration::from_millis(u64::from(timeout_ms) + 2_000))
            .send()
            .await?
            .error_for_status()?;
        Ok(response.json().await?)
    }

    /// Reads the first item from Mihomo's chunked `/traffic` stream.
    ///
    /// The endpoint stays open indefinitely, so reading the whole response body
    /// would always time out. Each poll intentionally closes the stream after
    /// one complete JSON object has been decoded.
    pub async fn traffic_sample(&self) -> eyre::Result<Traffic> {
        let mut response = self
            .build_request(Method::GET, "/traffic")
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?;
        let mut buffer = Vec::with_capacity(256);

        while let Some(chunk) = response.chunk().await? {
            buffer.extend_from_slice(&chunk);
            if let Some(traffic) = parse_first_traffic(&buffer)? {
                return Ok(traffic);
            }
            if buffer.len() > MAX_STREAM_BUFFER {
                return Err(eyre::eyre!("Mihomo traffic response exceeded 64 KiB"));
            }
        }

        Err(eyre::eyre!(
            "Mihomo traffic stream ended before returning a sample"
        ))
    }
}

fn parse_first_traffic(bytes: &[u8]) -> eyre::Result<Option<Traffic>> {
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines() {
        let candidate = line.strip_prefix("data:").unwrap_or(line).trim();
        if candidate.is_empty() {
            continue;
        }
        match serde_json::from_str(candidate) {
            Ok(traffic) => return Ok(Some(traffic)),
            Err(_) if !line.ends_with('}') => continue,
            Err(error) => return Err(error.into()),
        }
    }

    let candidate = text.strip_prefix("data:").unwrap_or(text.as_ref()).trim();
    if candidate.ends_with('}') {
        return Ok(Some(serde_json::from_str(candidate)?));
    }
    Ok(None)
}

fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];

        loop {
            let length = socket.read(&mut chunk).await.unwrap();
            if length == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..length]);

            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let header_end = header_end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or_default();

            if request.len() >= header_end + content_length {
                break;
            }
        }

        request
    }

    #[test]
    fn parses_plain_and_sse_traffic_samples() {
        assert_eq!(
            parse_first_traffic(br#"{"up":1024,"down":2048}"#).unwrap(),
            Some(Traffic {
                up: 1024,
                down: 2048
            })
        );
        assert_eq!(
            parse_first_traffic(b"data: {\"up\":1,\"down\":2}\n\n").unwrap(),
            Some(Traffic { up: 1, down: 2 })
        );
    }

    #[test]
    fn waits_for_a_complete_stream_item() {
        assert_eq!(parse_first_traffic(br#"{"up":12"#).unwrap(), None);
    }

    #[test]
    fn rejects_complete_invalid_json() {
        assert!(parse_first_traffic(b"{not-json}").is_err());
    }

    #[test]
    fn encodes_proxy_names_as_single_url_segments() {
        assert_eq!(encode_path_segment("香港/01"), "%E9%A6%99%E6%B8%AF%2F01");
        assert_eq!(encode_path_segment("Proxy-A_1"), "Proxy-A_1");
    }

    #[test]
    fn decodes_the_standard_mihomo_proxy_shape() {
        let snapshot: ProxySnapshot = serde_json::from_str(
            r#"{
                "proxies": {
                    "GLOBAL": {
                        "name": "GLOBAL",
                        "type": "Selector",
                        "now": "Node A",
                        "all": ["Node A", "DIRECT"],
                        "alive": true,
                        "history": []
                    },
                    "Node A": {
                        "name": "Node A",
                        "type": "Shadowsocks",
                        "alive": true,
                        "history": [{"time":"now","delay":86}]
                    }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(snapshot.proxies["GLOBAL"].now, "Node A");
        assert_eq!(snapshot.proxies["Node A"].history[0].delay, 86);
    }

    #[tokio::test]
    async fn rejects_an_unknown_mode_without_network_access() {
        let api = ClashApi::new("http://127.0.0.1:1".to_owned(), None);
        assert!(api.set_mode("invalid").await.is_err());
    }

    #[tokio::test]
    async fn version_uses_the_bearer_token_and_checks_the_http_contract() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 2048];
            let length = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..length]);
            assert!(request.starts_with("GET /version HTTP/1.1\r\n"));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-secret\r\n"));

            let body = r#"{"meta":true,"version":"1.19.0"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let api = ClashApi::new(format!("http://{address}/"), Some("test-secret".to_owned()));
        assert_eq!(
            api.version().await.unwrap(),
            Version {
                meta: Some(true),
                version: Some("1.19.0".to_owned())
            }
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn proxy_selection_encodes_the_group_and_sends_the_node_name() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            let request = String::from_utf8_lossy(&request);
            assert!(request
                .starts_with("PUT /proxies/%E9%A6%99%E6%B8%AF%2F%E8%87%AA%E9%80%89 HTTP/1.1\r\n"));
            assert!(request.contains(r#"{"name":"Node A"}"#));
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });

        let api = ClashApi::new(format!("http://{address}"), None);
        api.select_proxy("香港/自选", "Node A").await.unwrap();
        server.await.unwrap();
    }
}
