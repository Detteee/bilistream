use std::io;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// A small pool of clients keyed by proxy. Client clones share connections;
/// proxy changes cannot grow this cache without bound.
pub fn pooled_client(proxy: Option<&str>) -> Result<reqwest::Client, reqwest::Error> {
    type ClientEntry = (Option<String>, reqwest::Client);
    static CLIENTS: OnceLock<Mutex<Vec<ClientEntry>>> = OnceLock::new();
    let proxy = proxy.filter(|proxy| !proxy.is_empty());
    let mut clients = CLIENTS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some((_, client)) = clients.iter().find(|(key, _)| key.as_deref() == proxy) {
        return Ok(client.clone());
    }
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    let client = builder.build()?;
    if clients.len() == 4 {
        clients.remove(0);
    }
    clients.push((proxy.map(str::to_owned), client.clone()));
    Ok(client)
}

/// Enforce the limit while streaming, including responses without a length.
pub async fn response_bytes_limited(
    mut response: reqwest::Response,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let oversized = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "upstream response exceeds byte limit",
        )
    };
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(oversized());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(oversized());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub async fn response_json_limited<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> io::Result<T> {
    let bytes = response_bytes_limited(response, 8 * 1024 * 1024).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn caps_chunked_response_without_content_length() {
        crate::install_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(|| async {
                axum::body::Body::from_stream(futures_util::stream::iter(
                    (0..4).map(|_| Ok::<_, io::Error>(vec![b'x'; 512])),
                ))
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let response = pooled_client(None)
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.content_length(), None);
        let error = response_bytes_limited(response, 1024).await.unwrap_err();
        server.abort();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
