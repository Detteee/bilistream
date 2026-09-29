use ring::{aead, pbkdf2};
use std::io;
use std::num::NonZeroU32;
use zeroize::Zeroize;

const ENVELOPE: &[u8; 8] = b"BSTRSEC1";
const BACKUP: &[u8; 8] = b"BSTRBAK1";
const KDF_ROUNDS: u32 = 600_000;

pub(super) struct Key(pub [u8; 32]);

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub(super) fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|_| io::Error::other("无法生成安全随机数据"))?;
    Ok(bytes)
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "密钥不匹配或加密数据已损坏")
}

pub(super) fn seal(key: &Key, context: &[u8], plaintext: &[u8]) -> io::Result<Vec<u8>> {
    let nonce = random::<12>()?;
    let cipher = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, &key.0).map_err(|_| invalid())?,
    );
    let mut payload = plaintext.to_vec();
    cipher
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(context),
            &mut payload,
        )
        .map_err(|_| invalid())?;
    let mut result = Vec::with_capacity(20 + payload.len());
    result.extend_from_slice(ENVELOPE);
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&payload);
    Ok(result)
}

pub(super) fn open(key: &Key, context: &[u8], ciphertext: &[u8]) -> io::Result<Vec<u8>> {
    if ciphertext.len() < 36 || &ciphertext[..8] != ENVELOPE {
        return Err(invalid());
    }
    let nonce: [u8; 12] = ciphertext[8..20].try_into().map_err(|_| invalid())?;
    let cipher = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, &key.0).map_err(|_| invalid())?,
    );
    let mut payload = ciphertext[20..].to_vec();
    let plain = cipher
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(context),
            &mut payload,
        )
        .map_err(|_| invalid())?;
    Ok(plain.to_vec())
}

fn backup_key(password: &str, salt: &[u8]) -> io::Result<Key> {
    if password.chars().count() < 12 || password.len() > 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "备份密码至少需要 12 个字符",
        ));
    }
    let mut key = Key([0; 32]);
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(KDF_ROUNDS).expect("nonzero KDF rounds"),
        salt,
        password.as_bytes(),
        &mut key.0,
    );
    Ok(key)
}

pub(super) fn export(password: &str, data: &[u8]) -> io::Result<Vec<u8>> {
    let salt = random::<16>()?;
    let key = backup_key(password, &salt)?;
    let mut output = BACKUP.to_vec();
    output.extend_from_slice(&salt);
    output.extend_from_slice(&seal(&key, BACKUP, data)?);
    Ok(output)
}

pub(super) fn restore(password: &str, data: &[u8]) -> io::Result<Vec<u8>> {
    if data.len() < 60 || &data[..8] != BACKUP {
        return Err(invalid());
    }
    let key = backup_key(password, &data[8..24])?;
    open(&key, BACKUP, &data[24..])
}
