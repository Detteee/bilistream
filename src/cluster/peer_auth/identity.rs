use super::{denied, invalid};
use crate::storage::{Store, Transaction};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use zeroize::Zeroizing;

pub const IDENTITY_RECORD: &str = "cluster-identity";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicIdentity {
    pub member_id: String,
    pub public_key: String,
}

impl PublicIdentity {
    pub fn validate(&self) -> io::Result<()> {
        let bytes = decode::<32>(&self.public_key)?;
        if self.member_id != digest_hex(&bytes) {
            return Err(denied());
        }
        Ok(())
    }
}

/// Never derive Serialize/Debug: the private key may only enter the encrypted
/// internal identity document, never a response, export, error or log.
pub struct NodeIdentity {
    pair: Ed25519KeyPair,
    public: PublicIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    Hello,
    Request,
    Response,
    Proposal,
    Prepare,
    Decision,
    Installed,
    Finish,
    Pairing,
    /// Explicit "is this key still a member" answer. Not a request for peer authority.
    Recognition,
    /// A source's durable guarantee that execution stops within a hard bound.
    FencingPolicy,
}

impl Domain {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Hello => b"hello",
            Self::Request => b"request",
            Self::Response => b"response",
            Self::Proposal => b"proposal",
            Self::Prepare => b"prepare",
            Self::Decision => b"decision",
            Self::Installed => b"installed",
            Self::Finish => b"finish",
            Self::Pairing => b"pairing",
            Self::Recognition => b"recognition",
            Self::FencingPolicy => b"fencing-policy",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedProof {
    pub signer: String,
    pub signature: String,
}

impl NodeIdentity {
    pub fn public(&self) -> &PublicIdentity {
        &self.public
    }

    pub fn load(store: &Store) -> io::Result<Option<Self>> {
        store
            .read(IDENTITY_RECORD)?
            .map(|d| Self::from_record(d.value))
            .transpose()
    }

    pub fn load_from(tx: &Transaction<'_>) -> io::Result<Option<Self>> {
        tx.read(IDENTITY_RECORD)?
            .map(|d| Self::from_record(d.value))
            .transpose()
    }

    /// Only explicit create/join may call this, inside their hold transaction.
    /// Loading a missing/corrupt managed key must never call this as recovery.
    pub fn create_in(tx: &mut Transaction<'_>) -> io::Result<Self> {
        if let Some(identity) = Self::load_from(tx)? {
            return Ok(identity);
        }
        if tx.read("cluster-membership")?.is_some()
            || tx
                .read("cluster-local")?
                .is_some_and(|doc| doc.value["lifecycle"].as_str() != Some("incompatible_held"))
        {
            return Err(invalid("本地节点身份缺失，不能自动重建"));
        }
        Self::generate_in(tx)
    }

    /// Explicit re-enrollment after durable departure uses a fresh fingerprint.
    /// Retired nodes cannot recover their former authority by reusing a name.
    pub fn rotate_left_in(tx: &mut Transaction<'_>) -> io::Result<Self> {
        let local = tx
            .read("cluster-local")?
            .ok_or_else(|| invalid("节点尚未退出集群"))?;
        if local.value["lifecycle"].as_str() != Some("left")
            || !local.value["reservation"].is_null()
            || !local.value["pairing"].is_null()
            || Self::load_from(tx)?.is_none()
        {
            return Err(invalid("只有已退出且无未完成操作的节点可以更换身份"));
        }
        Self::generate_in(tx)
    }

    fn generate_in(tx: &mut Transaction<'_>) -> io::Result<Self> {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| invalid("无法创建节点身份"))?;
        let pair =
            Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| invalid("节点私钥格式无效"))?;
        let public = PublicIdentity {
            member_id: digest_hex(pair.public_key().as_ref()),
            public_key: URL_SAFE_NO_PAD.encode(pair.public_key().as_ref()),
        };
        tx.write(
            IDENTITY_RECORD,
            serde_json::json!({
                "version": 1,
                "member_id": public.member_id,
                "public_key": public.public_key,
                "private_key": URL_SAFE_NO_PAD.encode(pkcs8.as_ref()),
            }),
        )?;
        Ok(Self { pair, public })
    }

    fn from_record(value: serde_json::Value) -> io::Result<Self> {
        if value["version"].as_u64() != Some(1) {
            return Err(invalid("节点身份版本不兼容"));
        }
        let private = value["private_key"]
            .as_str()
            .ok_or_else(|| invalid("节点私钥缺失"))?;
        if private.len() > 512 {
            return Err(invalid("节点私钥格式无效"));
        }
        let pkcs8 = Zeroizing::new(
            URL_SAFE_NO_PAD
                .decode(private)
                .map_err(|_| invalid("节点私钥格式无效"))?,
        );
        let pair = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| invalid("节点私钥校验失败"))?;
        let public = PublicIdentity {
            member_id: digest_hex(pair.public_key().as_ref()),
            public_key: URL_SAFE_NO_PAD.encode(pair.public_key().as_ref()),
        };
        if value["member_id"].as_str() != Some(&public.member_id)
            || value["public_key"].as_str() != Some(&public.public_key)
        {
            return Err(invalid("节点身份与私钥不匹配"));
        }
        Ok(Self { pair, public })
    }

    pub fn sign(&self, domain: Domain, bytes: &[u8]) -> SignedProof {
        SignedProof {
            signer: self.public.member_id.clone(),
            signature: URL_SAFE_NO_PAD
                .encode(self.pair.sign(&signing_bytes(domain, bytes)).as_ref()),
        }
    }
}

pub fn verify_proof(
    public: &PublicIdentity,
    domain: Domain,
    bytes: &[u8],
    proof: &SignedProof,
) -> io::Result<()> {
    public.validate()?;
    if proof.signer != public.member_id {
        return Err(denied());
    }
    let key = decode::<32>(&public.public_key)?;
    let signature = decode::<64>(&proof.signature)?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&signing_bytes(domain, bytes), &signature)
        .map_err(|_| denied())
}

fn signing_bytes(domain: Domain, bytes: &[u8]) -> Vec<u8> {
    let mut result = Encoder::new();
    result.field(b"bilistream-node-v1");
    result.field(domain.bytes());
    result.field(bytes);
    result.finish()
}

pub fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn random_id() -> io::Result<String> {
    Ok(random::<16>()?.iter().map(|b| format!("{b:02x}")).collect())
}

pub(super) fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut result = [0; N];
    SystemRandom::new()
        .fill(&mut result)
        .map_err(|_| invalid("安全随机数不可用"))?;
    Ok(result)
}

pub(super) fn decode<const N: usize>(value: &str) -> io::Result<[u8; N]> {
    if value.len() > N * 2 {
        return Err(denied());
    }
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| denied())?
        .try_into()
        .map_err(|_| denied())
}

/// Stable length-delimited encoding for membership proposals and receipts too.
/// Encode fields in their protocol-defined order; sort collections first.
#[derive(Default)]
pub struct Encoder(Vec<u8>);

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn field(&mut self, bytes: &[u8]) {
        self.0
            .extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        self.0.extend_from_slice(bytes);
    }
    pub fn text(&mut self, value: &str) {
        self.field(value.as_bytes());
    }
    pub fn number(&mut self, value: u64) {
        self.field(&value.to_be_bytes());
    }
    pub fn finish(self) -> Vec<u8> {
        self.0
    }
}
