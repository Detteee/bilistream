use super::{
    invalid, sign_request, verify_response, HelloRequest, HelloResponse, NodeIdentity,
    ObservedHello, PeerScope, PublicIdentity, RoutePolicy, HELLO_RESPONSE_BYTES,
    PAIRING_BODY_BYTES,
};
use axum::http::{header, HeaderMap};
use reqwest::{Client, Method, Url};
use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

pub const HELLO_ROUTE: &str = "/api/cluster/v1/hello";
pub const PAIRING_ROUTE: &str = "/api/cluster/v1/pairing/reserve";
/// Signed membership probe. The caller's stored revision is not required to be current.
pub const RECOGNITION_ROUTE: &str = "/api/cluster/v1/membership/recognition";

/// Attach to the actual router's compression layer. Signed responses must keep
/// their exact bytes even if a client/proxy requests compression.
#[derive(Clone, Copy, Debug)]
pub struct PeerCompression;

impl tower_http::compression::predicate::Predicate for PeerCompression {
    fn should_compress<B: axum::body::HttpBody>(&self, response: &axum::http::Response<B>) -> bool {
        !response.headers().contains_key(header::AUTHORIZATION)
            && tower_http::compression::predicate::DefaultPredicate::new().should_compress(response)
    }
}

/// A validated advertised endpoint. HTTP is allowed only when the operator has
/// explicitly selected a previously established authenticated loopback tunnel.
#[derive(Clone, Debug)]
pub struct PeerEndpoint(Url);

impl PeerEndpoint {
    pub fn parse(value: &str, authenticated_loopback_tunnel: bool) -> io::Result<Self> {
        if value.len() > 2048 || value.contains('\\') || value.chars().any(char::is_whitespace) {
            return Err(invalid("节点地址必须使用 HTTPS 或已建立的本地认证隧道"));
        }
        let url = Url::parse(value).map_err(|_| invalid("节点地址格式无效"))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.host_str().is_none()
        {
            return Err(invalid("节点地址不能包含凭据、查询参数或片段"));
        }
        let literal_loopback = url
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        if url.scheme() != "https"
            && !(url.scheme() == "http" && authenticated_loopback_tunnel && literal_loopback)
        {
            return Err(invalid("节点地址必须使用 HTTPS 或已建立的本地认证隧道"));
        }
        Ok(Self(url))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn route_url(&self, logical_route: &str) -> io::Result<Url> {
        if !(logical_route.starts_with("/api/cluster/") || logical_route == "/api/server/restart")
            || logical_route.contains(['?', '#', '\\', '%'])
            || logical_route.contains("..")
            || logical_route.contains("//")
        {
            return Err(invalid("节点请求路径无效"));
        }
        let mut url = self.0.clone();
        url.set_path(&format!(
            "{}{}",
            self.0.path().trim_end_matches('/'),
            logical_route
        ));
        Ok(url)
    }
}

pub struct PeerResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Pool connections, never bearer credentials. Each request signs the caller's
/// current identity and membership snapshot. No redirects, cookies or decoding.
pub struct PeerClient {
    client: Client,
    hellos: Mutex<HashMap<String, ObservedHello>>,
}

impl PeerClient {
    pub fn new(timeout: Duration) -> io::Result<Self> {
        crate::install_crypto_provider();
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .default_headers({
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::ACCEPT_ENCODING,
                    "identity"
                        .parse()
                        .map_err(|_| invalid("节点客户端初始化失败"))?,
                );
                headers
            })
            .build()
            .map_err(|_| invalid("节点客户端初始化失败"))?;
        Ok(Self {
            client,
            hellos: Mutex::new(HashMap::new()),
        })
    }

    pub async fn hello(
        &self,
        endpoint: &PeerEndpoint,
        expected: &PublicIdentity,
    ) -> io::Result<ObservedHello> {
        expected.validate()?;
        let key = format!("{}|{}", expected.member_id, endpoint.as_str());
        if let Some(hello) = self
            .hellos
            .lock()
            .map_err(|_| invalid("节点握手缓存不可用"))?
            .get(&key)
            .filter(|hello| hello.is_fresh())
            .cloned()
        {
            return Ok(hello);
        }
        let (hello, challenge) = self.fetch_hello(endpoint).await?;
        let hello = hello.verify(expected, &challenge)?;
        let mut cache = self
            .hellos
            .lock()
            .map_err(|_| invalid("节点握手缓存不可用"))?;
        cache.retain(|_, hello| hello.is_fresh());
        if cache.len() >= 128 {
            cache.clear();
        }
        cache.insert(key, hello.clone());
        Ok(hello)
    }

    fn forget(&self, endpoint: &PeerEndpoint, expected: &PublicIdentity) {
        if let Ok(mut cache) = self.hellos.lock() {
            cache.remove(&format!("{}|{}", expected.member_id, endpoint.as_str()));
        }
    }

    /// Discover a candidate through verified transport before pairing. This
    /// proves possession of its key, not cluster membership or panel authority.
    pub async fn discover(&self, endpoint: &PeerEndpoint) -> io::Result<PublicIdentity> {
        let (hello, challenge) = self.fetch_hello(endpoint).await?;
        let identity = hello.identity.clone();
        hello.verify(&identity, &challenge)?;
        Ok(identity)
    }

    async fn fetch_hello(
        &self,
        endpoint: &PeerEndpoint,
    ) -> io::Result<(HelloResponse, HelloRequest)> {
        let challenge = HelloRequest::new()?;
        let response = self
            .client
            .post(endpoint.route_url(HELLO_ROUTE)?)
            .json(&challenge)
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() != reqwest::StatusCode::OK {
            // Gateway error bodies are not handshake evidence, even when a
            // tunnel compresses them despite Accept-Encoding: identity.
            return Err(unsigned_error(response.status().as_u16()));
        }
        if response.headers().contains_key(header::CONTENT_ENCODING) {
            return Err(invalid("节点握手失败"));
        }
        let bytes = crate::plugins::http::response_bytes_limited(response, HELLO_RESPONSE_BYTES)
            .await
            .map_err(|_| invalid("节点握手响应无效或过大"))?;
        let hello: HelloResponse =
            serde_json::from_slice(&bytes).map_err(|_| invalid("节点握手响应无效"))?;
        Ok((hello, challenge))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        &self,
        identity: &NodeIdentity,
        endpoint: &PeerEndpoint,
        expected: &PublicIdentity,
        cluster_id: &str,
        revision: u64,
        scope: PeerScope,
        route: RoutePolicy,
        body: Vec<u8>,
    ) -> io::Result<PeerResponse> {
        if body.len() > route.max_request_bytes {
            return Err(invalid("节点请求过大"));
        }
        let mut attempt = 0;
        let (signed, response) = loop {
            let hello = self.hello(endpoint, expected).await?;
            let signed = sign_request(
                identity,
                &hello,
                cluster_id,
                revision,
                scope.clone(),
                route,
                &body,
            )?;
            let response = self
                .client
                .request(
                    Method::from_bytes(route.method.as_bytes())
                        .map_err(|_| invalid("节点请求方法无效"))?,
                    endpoint.route_url(route.path)?,
                )
                .header(header::AUTHORIZATION, signed.authorization.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send()
                .await
                .map_err(transport_error)?;
            let headers = response.headers().clone();
            #[cfg(test)]
            let headers = {
                let mut headers = headers;
                if response_fault(endpoint) == Some(ResponseFault::Unsigned) {
                    headers.remove(header::AUTHORIZATION);
                }
                headers
            };
            if headers.contains_key(header::AUTHORIZATION) {
                break (signed, response);
            }
            let status = response.status().as_u16();
            // A restarted receiver rejects the cached boot nonce; refresh once.
            if status == 401 && attempt == 0 {
                attempt += 1;
                self.forget(endpoint, expected);
                continue;
            }
            return Err(unsigned_error(status));
        };
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let content_type = content_type(&headers)?;
        let bytes =
            crate::plugins::http::response_bytes_limited(response, route.max_response_bytes)
                .await
                .map_err(|_| invalid("节点响应无效或过大"))?;
        #[cfg(test)]
        let bytes = {
            let mut bytes = bytes;
            if response_fault(endpoint) == Some(ResponseFault::TamperBody) {
                // Change the signature text without breaking JSON, so a parser
                // that skips signature checks would still see "absent".
                let at = bytes
                    .windows(12)
                    .position(|window| window == b"\"signature\":")
                    .map(|pos| pos + 14)
                    .unwrap_or(0);
                if let Some(byte) = bytes.get_mut(at) {
                    *byte ^= 1;
                }
            }
            bytes
        };
        verify_response(
            &signed,
            expected,
            &headers,
            status,
            &content_type,
            &bytes,
            route,
        )?;
        Ok(PeerResponse {
            status,
            content_type,
            body: bytes,
        })
    }

    /// The sole password-bearing peer call. Caller must persist enrollment
    /// intent first and verify the returned target-signed pairing receipt.
    /// This does not issue or store a browser session or retry the password.
    pub async fn reserve_pairing(
        &self,
        endpoint: &PeerEndpoint,
        body: Vec<u8>,
    ) -> io::Result<PeerResponse> {
        if body.len() > PAIRING_BODY_BYTES {
            return Err(invalid("配对请求过大"));
        }
        let response = self
            .client
            .post(endpoint.route_url(PAIRING_ROUTE)?)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(transport_error)?;
        if response.status().is_redirection()
            || response.headers().contains_key(header::CONTENT_ENCODING)
        {
            return Err(invalid("配对响应无效"));
        }
        let status = response.status().as_u16();
        let content_type = content_type(response.headers())?;
        let body =
            crate::plugins::http::response_bytes_limited(response, super::OPERATION_BODY_BYTES)
                .await
                .map_err(|_| invalid("配对响应无效或过大"))?;
        Ok(PeerResponse {
            status,
            content_type,
            body,
        })
    }
}

/// Unsigned answers never carry protocol meaning. Only the tunnel/origin-down
/// statuses are classified as unreachable for the failover rules.
pub(crate) fn unsigned_error(status: u16) -> io::Error {
    if matches!(status, 502..=504 | 520..=530) {
        io::Error::new(io::ErrorKind::ConnectionAborted, "节点上游暂不可用")
    } else {
        io::Error::new(io::ErrorKind::PermissionDenied, "节点拒绝请求或响应未签名")
    }
}

fn content_type(headers: &HeaderMap) -> io::Result<String> {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let value = values
        .next()
        .ok_or_else(|| invalid("节点响应缺少内容类型"))?;
    if values.next().is_some() {
        return Err(invalid("节点响应内容类型无效"));
    }
    value
        .to_str()
        .map(str::to_owned)
        .map_err(|_| invalid("节点响应内容类型无效"))
}

/// Test-only substitution of one peer's next signed responses. Keyed by the
/// advertised endpoint so parallel tests do not share a fault.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseFault {
    /// Drop the response signature. An unsigned body is not a membership proof.
    Unsigned,
    /// Flip the response body under the original signature.
    TamperBody,
}

#[cfg(test)]
fn response_faults() -> &'static Mutex<HashMap<String, ResponseFault>> {
    static FAULTS: std::sync::OnceLock<Mutex<HashMap<String, ResponseFault>>> =
        std::sync::OnceLock::new();
    FAULTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn fault_key(endpoint: &PeerEndpoint) -> String {
    endpoint.as_str().trim_end_matches('/').to_owned()
}

#[cfg(test)]
pub fn set_response_fault(endpoint: &str, fault: Option<ResponseFault>) {
    let key = endpoint.trim_end_matches('/').to_owned();
    let mut faults = response_faults()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match fault {
        Some(fault) => {
            faults.insert(key, fault);
        }
        None => {
            faults.remove(&key);
        }
    }
}

#[cfg(test)]
fn response_fault(endpoint: &PeerEndpoint) -> Option<ResponseFault> {
    response_faults()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&fault_key(endpoint))
        .copied()
}

fn transport_error(error: reqwest::Error) -> io::Error {
    // No URL, peer address, password-bearing request, or upstream body in errors.
    let kind = if error.is_timeout() {
        io::ErrorKind::TimedOut
    } else if error.is_connect() {
        io::ErrorKind::ConnectionRefused
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(kind, "节点连接失败，请检查地址、隧道和服务状态")
}
