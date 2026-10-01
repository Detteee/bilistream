use super::identity::{decode, random};
use super::{
    denied, digest_hex, invalid, verify_proof, Domain, Encoder, NodeIdentity, PublicIdentity,
    SignedProof,
};
use axum::http::{header, HeaderMap, HeaderValue};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const AUTH_SCHEME: &str = "Bilistream-Node-v1 ";
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
pub const OPERATION_BODY_BYTES: usize = 256 * 1024;
pub const PAIRING_BODY_BYTES: usize = 1024 * 1024;
pub const HELLO_REQUEST_BYTES: usize = 1024;
pub const HELLO_RESPONSE_BYTES: usize = 4096;
const MAX_AUTH_BYTES: usize = 8192;
const MAX_AGE_MS: u64 = 120_000;
const MAX_FUTURE_MS: u64 = 30_000;
const PER_MEMBER_REPLAYS: usize = 4096;
const TOTAL_REPLAYS: usize = 65_536;

/// The router selects this from its own exact method/path table, never from
/// Host, forwarded headers, or an untrusted logical-route header.
#[derive(Clone, Copy, Debug)]
pub struct RoutePolicy {
    pub method: &'static str,
    pub path: &'static str,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
}

/// The request the local router already admitted. `route` comes from the
/// router's own table; method and path are the request as received.
pub struct AdmittedRequest<'a> {
    pub headers: &'a HeaderMap,
    pub method: &'a str,
    pub path_and_query: &'a str,
    pub route: RoutePolicy,
    pub body: &'a [u8],
}

impl RoutePolicy {
    fn validate(&self) -> io::Result<()> {
        if !matches!(self.method, "GET" | "POST")
            || !(self.path.starts_with("/api/cluster/")
                || (self.path == "/api/server/restart" && self.method == "POST"))
            || self.path.contains(['?', '#', '%', '\\'])
            || self.path.contains("//")
            || self.path.ends_with('/')
        {
            return Err(denied());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "operation_id", rename_all = "snake_case")]
pub enum PeerScope {
    Ordinary,
    Operation(String),
}

/// Build this from one committed snapshot. An operation grant must contain only
/// the exact operation's participants/coordinator and its admissible revisions.
pub struct TrustSnapshot {
    pub cluster_id: String,
    pub revisions: Vec<u64>,
    pub scope: PeerScope,
    pub members: Vec<PublicIdentity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloRequest {
    pub challenge: String,
}

impl HelloRequest {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            challenge: URL_SAFE_NO_PAD.encode(random::<32>()?),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloResponse {
    pub protocol: u64,
    pub identity: PublicIdentity,
    pub challenge: String,
    pub boot_nonce: String,
    pub elapsed_ms: u64,
    pub proof: SignedProof,
}

impl HelloResponse {
    fn signing_bytes(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.number(self.protocol);
        e.text(&self.identity.member_id);
        e.text(&self.identity.public_key);
        e.text(&self.challenge);
        e.text(&self.boot_nonce);
        e.number(self.elapsed_ms);
        e.finish()
    }

    pub fn verify(
        self,
        expected: &PublicIdentity,
        challenge: &HelloRequest,
    ) -> io::Result<ObservedHello> {
        decode::<32>(&self.challenge)?;
        decode::<32>(&self.boot_nonce)?;
        if self.protocol != 1 || &self.identity != expected || self.challenge != challenge.challenge
        {
            return Err(denied());
        }
        verify_proof(expected, Domain::Hello, &self.signing_bytes(), &self.proof)?;
        Ok(ObservedHello {
            hello: self,
            observed: Instant::now(),
        })
    }
}

#[derive(Clone)]
pub struct ObservedHello {
    hello: HelloResponse,
    observed: Instant,
}

impl ObservedHello {
    pub fn is_fresh(&self) -> bool {
        self.observed.elapsed() < Duration::from_secs(60)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestMetadata {
    pub cluster_id: String,
    pub sender: String,
    pub recipient: String,
    pub revision: u64,
    pub scope: PeerScope,
    pub boot_nonce: String,
    pub request_nonce: String,
    pub issued_ms: u64,
    pub method: String,
    pub route: String,
    pub body_len: u64,
    pub body_hash: String,
}

impl RequestMetadata {
    fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.number(1);
        e.text(&self.cluster_id);
        e.text(&self.sender);
        e.text(&self.recipient);
        e.number(self.revision);
        match &self.scope {
            PeerScope::Ordinary => {
                e.text("ordinary");
                e.text("");
            }
            PeerScope::Operation(id) => {
                e.text("operation");
                e.text(id);
            }
        }
        e.text(&self.boot_nonce);
        e.text(&self.request_nonce);
        e.number(self.issued_ms);
        e.text(&self.method);
        e.text(&self.route);
        e.number(self.body_len);
        e.text(&self.body_hash);
        e.finish()
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut d = Decoder(bytes);
        if d.number()? != 1 {
            return Err(denied());
        }
        let cluster_id = d.text(64)?;
        let sender = d.text(64)?;
        let recipient = d.text(64)?;
        let revision = d.number()?;
        let kind = d.text(16)?;
        let operation = d.text(128)?;
        let scope = match (kind.as_str(), operation.as_str()) {
            ("ordinary", "") => PeerScope::Ordinary,
            ("operation", id) if !id.is_empty() => PeerScope::Operation(operation),
            _ => return Err(denied()),
        };
        let result = Self {
            cluster_id,
            sender,
            recipient,
            revision,
            scope,
            boot_nonce: d.text(64)?,
            request_nonce: d.text(64)?,
            issued_ms: d.number()?,
            method: d.text(8)?,
            route: d.text(256)?,
            body_len: d.number()?,
            body_hash: d.text(64)?,
        };
        if !d.0.is_empty() || result.cluster_id.is_empty() {
            return Err(denied());
        }
        decode::<32>(&result.boot_nonce)?;
        decode::<32>(&result.request_nonce)?;
        Ok(result)
    }
}

pub struct SignedRequest {
    pub authorization: HeaderValue,
    pub metadata: RequestMetadata,
}

/// Body of the narrow membership probe. The receiver verifies the request
/// signature with `public_key` and does not consult the caller's revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecognitionProbe {
    pub cluster_id: String,
    pub member_id: String,
    pub public_key: String,
}

impl RecognitionProbe {
    pub fn identity(&self) -> PublicIdentity {
        PublicIdentity {
            member_id: self.member_id.clone(),
            public_key: self.public_key.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecognitionVerdict {
    Present,
    Absent,
}

/// Receiver-signed answer. `proof` is the receiver's node key, checked by the
/// caller against the peer key already pinned in its stored manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecognitionAnswer {
    pub cluster_id: String,
    pub member_id: String,
    pub verdict: RecognitionVerdict,
    pub proof: SignedProof,
}

impl RecognitionAnswer {
    pub fn sign(
        cluster_id: &str,
        member_id: &str,
        verdict: RecognitionVerdict,
        identity: &NodeIdentity,
    ) -> Self {
        let mut answer = Self {
            cluster_id: cluster_id.to_owned(),
            member_id: member_id.to_owned(),
            verdict,
            proof: SignedProof {
                signer: String::new(),
                signature: String::new(),
            },
        };
        answer.proof = identity.sign(Domain::Recognition, &answer.signing_bytes());
        answer
    }

    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.number(1);
        encoder.text(&self.cluster_id);
        encoder.text(&self.member_id);
        encoder.text(match self.verdict {
            RecognitionVerdict::Present => "present",
            RecognitionVerdict::Absent => "absent",
        });
        encoder.finish()
    }

    pub fn verify(&self, signer: &PublicIdentity) -> io::Result<()> {
        verify_proof(
            signer,
            Domain::Recognition,
            &self.signing_bytes(),
            &self.proof,
        )
    }
}

/// Inserted only for the recognition route. It is not [`AuthenticatedPeer`]
/// authority and must not be treated as a cluster member admission.
#[derive(Clone, Debug)]
pub struct VerifiedRecognition(pub RecognitionProbe);

pub fn sign_request(
    identity: &NodeIdentity,
    receiver: &ObservedHello,
    cluster_id: &str,
    revision: u64,
    scope: PeerScope,
    route: RoutePolicy,
    body: &[u8],
) -> io::Result<SignedRequest> {
    route.validate()?;
    if !receiver.is_fresh() || body.len() > route.max_request_bytes {
        return Err(invalid("节点握手已过期或请求过大"));
    }
    let metadata = RequestMetadata {
        cluster_id: cluster_id.into(),
        sender: identity.public().member_id.clone(),
        recipient: receiver.hello.identity.member_id.clone(),
        revision,
        scope,
        boot_nonce: receiver.hello.boot_nonce.clone(),
        request_nonce: URL_SAFE_NO_PAD.encode(random::<32>()?),
        issued_ms: receiver
            .hello
            .elapsed_ms
            .saturating_add(elapsed_ms(receiver.observed)),
        method: route.method.into(),
        route: route.path.into(),
        body_len: body.len() as u64,
        body_hash: digest_hex(body),
    };
    let encoded = metadata.encode();
    // Refuse to emit a representation the receiving decoder would reject.
    RequestMetadata::decode(&encoded)?;
    let proof = identity.sign(Domain::Request, &encoded);
    Ok(SignedRequest {
        authorization: envelope(&encoded, &proof)?,
        metadata,
    })
}

/// Construct once per listener process start, before accepting any requests.
pub struct PeerReceiver {
    boot_nonce: String,
    started: Instant,
    replay: Mutex<ReplayCache>,
}

impl PeerReceiver {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            boot_nonce: URL_SAFE_NO_PAD.encode(random::<32>()?),
            started: Instant::now(),
            replay: Mutex::new(ReplayCache::default()),
        })
    }

    pub fn hello(
        &self,
        identity: &NodeIdentity,
        request: &HelloRequest,
    ) -> io::Result<HelloResponse> {
        decode::<32>(&request.challenge)?;
        let mut hello = HelloResponse {
            protocol: 1,
            identity: identity.public().clone(),
            challenge: request.challenge.clone(),
            boot_nonce: self.boot_nonce.clone(),
            elapsed_ms: elapsed_ms(self.started),
            proof: SignedProof {
                signer: String::new(),
                signature: String::new(),
            },
        };
        hello.proof = identity.sign(Domain::Hello, &hello.signing_bytes());
        Ok(hello)
    }

    pub fn authenticate(
        &self,
        request: &AdmittedRequest<'_>,
        local: &PublicIdentity,
        trust: &TrustSnapshot,
    ) -> io::Result<AuthenticatedPeer> {
        self.authenticate_at(
            request.headers,
            request.method,
            request.path_and_query,
            request.route,
            request.body,
            local,
            trust,
            elapsed_ms(self.started),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn authenticate_at(
        &self,
        headers: &HeaderMap,
        actual_method: &str,
        actual_path_and_query: &str,
        route: RoutePolicy,
        body: &[u8],
        local: &PublicIdentity,
        trust: &TrustSnapshot,
        now: u64,
    ) -> io::Result<AuthenticatedPeer> {
        let (encoded, metadata, signature) = self.bound_request(
            headers,
            actual_method,
            actual_path_and_query,
            route,
            body,
            local,
            now,
        )?;
        if metadata.cluster_id != trust.cluster_id
            || metadata.scope != trust.scope
            || !trust.revisions.contains(&metadata.revision)
        {
            return Err(denied());
        }
        let public = trust
            .members
            .iter()
            .find(|p| p.member_id == metadata.sender)
            .ok_or_else(denied)?;
        self.admit(encoded, metadata, &signature, public, now)
    }

    /// Verify the caller with the public key inside the probe. A stale
    /// membership revision, which ordinary routes reject with the same 401 as a
    /// bad signature, is not consulted: the caller's stored revision is the one
    /// from before it was removed.
    pub fn authenticate_recognition(
        &self,
        request: &AdmittedRequest<'_>,
        local: &PublicIdentity,
    ) -> io::Result<(AuthenticatedPeer, RecognitionProbe)> {
        let (encoded, metadata, signature) = self.bound_request(
            request.headers,
            request.method,
            request.path_and_query,
            request.route,
            request.body,
            local,
            elapsed_ms(self.started),
        )?;
        if metadata.scope != PeerScope::Ordinary {
            return Err(denied());
        }
        let probe: RecognitionProbe = serde_json::from_slice(request.body).map_err(|_| denied())?;
        let public = probe.identity();
        public.validate()?;
        if probe.cluster_id != metadata.cluster_id || public.member_id != metadata.sender {
            return Err(denied());
        }
        let peer = self.admit(
            encoded,
            metadata,
            &signature,
            &public,
            elapsed_ms(self.started),
        )?;
        Ok((peer, probe))
    }

    #[allow(clippy::too_many_arguments)]
    fn bound_request(
        &self,
        headers: &HeaderMap,
        actual_method: &str,
        actual_path_and_query: &str,
        route: RoutePolicy,
        body: &[u8],
        local: &PublicIdentity,
        now: u64,
    ) -> io::Result<(Vec<u8>, RequestMetadata, String)> {
        route.validate()?;
        if actual_method != route.method
            || actual_path_and_query != route.path
            || body.len() > route.max_request_bytes
            || headers.contains_key(header::COOKIE)
            || headers.contains_key(header::CONTENT_ENCODING)
        {
            return Err(denied());
        }
        let (encoded, signature) = read_envelope(headers)?;
        let metadata = RequestMetadata::decode(&encoded)?;
        if metadata.recipient != local.member_id
            || metadata.method != route.method
            || metadata.route != route.path
            || metadata.body_len != body.len() as u64
            || metadata.body_hash != digest_hex(body)
            || metadata.boot_nonce != self.boot_nonce
            || metadata.issued_ms > now.saturating_add(MAX_FUTURE_MS)
            || now > metadata.issued_ms.saturating_add(MAX_AGE_MS)
        {
            return Err(denied());
        }
        Ok((encoded, metadata, signature))
    }

    fn admit(
        &self,
        encoded: Vec<u8>,
        metadata: RequestMetadata,
        signature: &str,
        public: &PublicIdentity,
        now: u64,
    ) -> io::Result<AuthenticatedPeer> {
        verify_proof(
            public,
            Domain::Request,
            &encoded,
            &SignedProof {
                signer: metadata.sender.clone(),
                signature: signature.to_owned(),
            },
        )?;
        // Only a valid signature may consume replay capacity.
        self.replay
            .lock()
            .map_err(|_| invalid("节点重放缓存不可用"))?
            .consume(
                &metadata.sender,
                &metadata.request_nonce,
                metadata.issued_ms.saturating_add(MAX_AGE_MS),
                now,
            )?;
        Ok(AuthenticatedPeer {
            metadata,
            request_hash: digest_hex(&encoded),
        })
    }
}

#[derive(Default)]
struct ReplayCache {
    entries: HashMap<String, HashMap<String, u64>>,
    total: usize,
    next_expiry: Option<u64>,
}

impl ReplayCache {
    fn consume(&mut self, sender: &str, nonce: &str, expires: u64, now: u64) -> io::Result<()> {
        if self.next_expiry.is_some_and(|expiry| now > expiry) {
            self.entries.retain(|_, entries| {
                entries.retain(|_, expiry| *expiry >= now);
                !entries.is_empty()
            });
            self.total = self.entries.values().map(HashMap::len).sum();
            self.next_expiry = self
                .entries
                .values()
                .flat_map(HashMap::values)
                .copied()
                .min();
        }
        if self.total >= TOTAL_REPLAYS {
            return Err(denied());
        }
        let entries = self.entries.entry(sender.into()).or_default();
        if entries.contains_key(nonce) || entries.len() >= PER_MEMBER_REPLAYS {
            return Err(denied());
        }
        entries.insert(nonce.into(), expires);
        self.total += 1;
        self.next_expiry = Some(self.next_expiry.unwrap_or(expires).min(expires));
        Ok(())
    }
}

/// Cryptographic admission only. Mutation handlers must recheck the same
/// identity/revision/scope against the committed state inside their transaction.
#[derive(Clone, Debug)]
pub struct AuthenticatedPeer {
    metadata: RequestMetadata,
    request_hash: String,
}

impl AuthenticatedPeer {
    pub fn member_id(&self) -> &str {
        &self.metadata.sender
    }
    pub fn revision(&self) -> u64 {
        self.metadata.revision
    }
    pub fn cluster_id(&self) -> &str {
        &self.metadata.cluster_id
    }
    pub fn scope(&self) -> &PeerScope {
        &self.metadata.scope
    }

    pub fn sign_response(
        &self,
        identity: &NodeIdentity,
        status: u16,
        content_type: &str,
        body: &[u8],
        route: RoutePolicy,
    ) -> io::Result<HeaderValue> {
        if identity.public().member_id != self.metadata.recipient
            || body.len() > route.max_response_bytes
        {
            return Err(denied());
        }
        let bytes = response_bytes(
            &self.request_hash,
            &self.metadata,
            status,
            content_type,
            body,
        );
        envelope(&bytes, &identity.sign(Domain::Response, &bytes))
    }
}

pub fn verify_response(
    request: &SignedRequest,
    expected: &PublicIdentity,
    headers: &HeaderMap,
    status: u16,
    content_type: &str,
    body: &[u8],
    route: RoutePolicy,
) -> io::Result<()> {
    if expected.member_id != request.metadata.recipient
        || body.len() > route.max_response_bytes
        || headers.contains_key(header::CONTENT_ENCODING)
    {
        return Err(denied());
    }
    let (encoded, signature) = read_envelope(headers)?;
    let wanted = response_bytes(
        &digest_hex(&request.metadata.encode()),
        &request.metadata,
        status,
        content_type,
        body,
    );
    if wanted != encoded {
        return Err(denied());
    }
    verify_proof(
        expected,
        Domain::Response,
        &encoded,
        &SignedProof {
            signer: expected.member_id.clone(),
            signature,
        },
    )
}

fn response_bytes(
    request_hash: &str,
    request: &RequestMetadata,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut e = Encoder::new();
    e.number(1);
    e.text(request_hash);
    e.text(&request.recipient);
    e.text(&request.sender);
    e.number(status as u64);
    e.text(content_type);
    e.number(body.len() as u64);
    e.text(&digest_hex(body));
    e.finish()
}

fn envelope(metadata: &[u8], proof: &SignedProof) -> io::Result<HeaderValue> {
    let mut e = Encoder::new();
    e.field(metadata);
    e.field(&decode::<64>(&proof.signature)?);
    let value = format!("{AUTH_SCHEME}{}", URL_SAFE_NO_PAD.encode(e.finish()));
    if value.len() > MAX_AUTH_BYTES {
        return Err(denied());
    }
    let mut value = HeaderValue::from_str(&value).map_err(|_| denied())?;
    value.set_sensitive(true);
    Ok(value)
}

fn read_envelope(headers: &HeaderMap) -> io::Result<(Vec<u8>, String)> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next().ok_or_else(denied)?;
    if values.next().is_some() {
        return Err(denied());
    }
    let value = value.to_str().map_err(|_| denied())?;
    if value.len() > MAX_AUTH_BYTES {
        return Err(denied());
    }
    let value = value.strip_prefix(AUTH_SCHEME).ok_or_else(denied)?;
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| denied())?;
    let mut d = Decoder(&bytes);
    let metadata = d.field(MAX_AUTH_BYTES)?.to_vec();
    let signature = d.field(64)?;
    if signature.len() != 64 || !d.0.is_empty() {
        return Err(denied());
    }
    Ok((metadata, URL_SAFE_NO_PAD.encode(signature)))
}

struct Decoder<'a>(&'a [u8]);
impl<'a> Decoder<'a> {
    fn field(&mut self, cap: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < 8 {
            return Err(denied());
        }
        let len = u64::from_be_bytes(self.0[..8].try_into().map_err(|_| denied())?);
        if len > cap as u64 || len > (self.0.len() - 8) as u64 {
            return Err(denied());
        }
        let result = &self.0[8..8 + len as usize];
        self.0 = &self.0[8 + len as usize..];
        Ok(result)
    }
    fn number(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(
            self.field(8)?.try_into().map_err(|_| denied())?,
        ))
    }
    fn text(&mut self, cap: usize) -> io::Result<String> {
        String::from_utf8(self.field(cap)?.to_vec()).map_err(|_| denied())
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod replay_tests {
    use super::*;

    #[test]
    fn receiver_relative_clock_bounds_and_invalid_signature_do_not_consume_nonce() {
        let directory = std::env::temp_dir().join(format!(
            "bilistream-peer-clock-{}",
            crate::cluster::peer_auth::random_id().unwrap()
        ));
        let store =
            crate::storage::Store::open(directory.join("data"), directory.join("key"), None)
                .unwrap();
        let identity = store.transaction(NodeIdentity::create_in).unwrap();
        let receiver = PeerReceiver::new().unwrap();
        let challenge = HelloRequest::new().unwrap();
        let observed = receiver
            .hello(&identity, &challenge)
            .unwrap()
            .verify(identity.public(), &challenge)
            .unwrap();
        let route = RoutePolicy {
            method: "POST",
            path: "/api/cluster/self-check",
            max_request_bytes: 1024,
            max_response_bytes: 4096,
        };
        let trust = TrustSnapshot {
            cluster_id: "cluster".into(),
            revisions: vec![1],
            scope: PeerScope::Ordinary,
            members: vec![identity.public().clone()],
        };
        for (issued, allowed) in [
            (79_999, false),
            (80_000, true),
            (230_000, true),
            (230_001, false),
        ] {
            let mut signed = sign_request(
                &identity,
                &observed,
                "cluster",
                1,
                PeerScope::Ordinary,
                route,
                b"{}",
            )
            .unwrap();
            signed.metadata.issued_ms = issued;
            let encoded = signed.metadata.encode();
            let proof = identity.sign(Domain::Request, &encoded);
            let mut headers = HeaderMap::new();
            let mut bad_proof = proof.clone();
            bad_proof.signature = URL_SAFE_NO_PAD.encode([0; 64]);
            headers.insert(
                header::AUTHORIZATION,
                envelope(&encoded, &bad_proof).unwrap(),
            );
            assert!(receiver
                .authenticate_at(
                    &headers,
                    "POST",
                    route.path,
                    route,
                    b"{}",
                    identity.public(),
                    &trust,
                    200_000
                )
                .is_err());
            headers.insert(header::AUTHORIZATION, envelope(&encoded, &proof).unwrap());
            assert_eq!(
                receiver
                    .authenticate_at(
                        &headers,
                        "POST",
                        route.path,
                        route,
                        b"{}",
                        identity.public(),
                        &trust,
                        200_000
                    )
                    .is_ok(),
                allowed
            );
        }
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn capacity_never_evicts_a_live_nonce_and_expiry_includes_boundary() {
        let mut cache = ReplayCache::default();
        for i in 0..PER_MEMBER_REPLAYS {
            cache.consume("a", &i.to_string(), 150_000, 0).unwrap();
        }
        assert!(cache.consume("a", "new", 150_000, 150_000).is_err());
        assert!(cache.consume("a", "0", 150_000, 150_000).is_err());
        cache.consume("a", "new", 300_000, 150_001).unwrap();
        let mut cache = ReplayCache::default();
        for member in 0..16 {
            for nonce in 0..PER_MEMBER_REPLAYS {
                cache
                    .consume(&member.to_string(), &nonce.to_string(), 300_000, 150_001)
                    .unwrap();
            }
        }
        assert!(cache
            .consume("overflow", "nonce", 300_000, 150_001)
            .is_err());
        assert!(cache.consume("0", "0", 300_000, 150_001).is_err());
    }
}
