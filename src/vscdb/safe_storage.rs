//! Electron `safeStorage` on macOS: AES-128-CBC with a key derived from a Keychain password,
//! serialized by VSCode-based apps as a JSON `Buffer`.

use aes::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

type Encryptor = cbc::Encryptor<aes::Aes128>;
type Decryptor = cbc::Decryptor<aes::Aes128>;

const PREFIX: &[u8] = b"v10";
const SALT: &[u8] = b"saltysalt";
const ITERATIONS: u32 = 1003;
const IV: [u8; 16] = [b' '; 16];

pub struct SafeStorage {
    key: [u8; 16],
}

impl SafeStorage {
    pub fn from_password(password: &[u8]) -> Self {
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, SALT, ITERATIONS, &mut key);
        Self { key }
    }

    /// Reads the app's "<App> Safe Storage" Keychain item; macOS asks the user to allow this.
    ///
    /// The account name varies between Electron versions ("Code" vs. "Code Key"), so when the
    /// expected account is not found, fall back to any generic password with that service.
    pub fn from_keychain(service: &str, account: &str) -> Result<Self> {
        let password = match security_framework::passwords::get_generic_password(service, account)
        {
            Ok(password) => password,
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => find_by_service(service)?,
            Err(e) => {
                return Err(anyhow!(e))
                    .with_context(|| format!("reading \"{service}\" from the Keychain"));
            }
        };
        Ok(Self::from_password(&password))
    }

    pub fn encrypt(&self, plaintext: &str) -> Vec<u8> {
        let ciphertext = Encryptor::new(&self.key.into(), &IV.into())
            .encrypt_padded_vec::<Pkcs7>(plaintext.as_bytes());
        [PREFIX, &ciphertext].concat()
    }

    pub fn decrypt(&self, data: &[u8]) -> Result<String> {
        let Some(ciphertext) = data.strip_prefix(PREFIX) else {
            bail!("secret is not in the v10 safeStorage format")
        };
        let plaintext = Decryptor::new(&self.key.into(), &IV.into())
            .decrypt_padded_vec::<Pkcs7>(ciphertext)
            .map_err(|_| anyhow!("could not decrypt the secret with the Keychain password"))?;
        String::from_utf8(plaintext).context("decrypted secret is not valid UTF-8")
    }
}

const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

fn find_by_service(service: &str) -> Result<Vec<u8>> {
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit, SearchResult};
    let results = ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(service)
        .load_data(true)
        .limit(Limit::Max(1))
        .search()
        .map_err(|e| anyhow!(e))
        .with_context(|| format!("reading \"{service}\" from the Keychain"))?;
    results
        .into_iter()
        .find_map(|result| match result {
            SearchResult::Data(data) => Some(data),
            _ => None,
        })
        .with_context(|| format!("reading \"{service}\" from the Keychain: no password data"))
}

/// `{"type":"Buffer","data":[...]}`, the shape `JSON.stringify(Buffer)` produces.
pub fn encode_buffer(bytes: &[u8]) -> String {
    json!({"type": "Buffer", "data": bytes}).to_string()
}

pub fn decode_buffer(text: &str) -> Result<Vec<u8>> {
    let value: Value = serde_json::from_str(text).context("secret is not a JSON Buffer")?;
    if value["type"] != "Buffer" {
        bail!("secret is not a JSON Buffer");
    }
    value["data"]
        .as_array()
        .context("secret Buffer has no data")?
        .iter()
        .map(|b| {
            b.as_u64()
                .filter(|b| *b <= 255)
                .map(|b| b as u8)
                .context("secret Buffer has a non-byte value")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let storage = SafeStorage::from_password(b"peanuts");
        let encrypted = storage.encrypt("sk-test-123");
        assert!(encrypted.starts_with(b"v10"));
        let stored = encode_buffer(&encrypted);
        assert!(stored.starts_with(r#"{"type":"Buffer","data":[118,49,48,"#));
        assert_eq!(
            storage.decrypt(&decode_buffer(&stored).unwrap()).unwrap(),
            "sk-test-123"
        );
    }

    /// Vectors from Python's hashlib.pbkdf2_hmac and `openssl enc -aes-128-cbc`.
    #[test]
    fn matches_reference_vectors() {
        let storage = SafeStorage::from_password(b"peanuts");
        assert_eq!(hex(&storage.key), "d9a09d499b4e1b7461f28e67972c6dbd");
        assert_eq!(
            hex(&storage.encrypt("sk-test-123")[3..]),
            "7c4405c0ce1ea8223a1ef47f777a92a1"
        );
    }

    #[test]
    fn rejects_other_formats() {
        let storage = SafeStorage::from_password(b"peanuts");
        assert!(storage.decrypt(b"v11abc").is_err());
        assert!(decode_buffer(r#"{"type":"String","data":[1]}"#).is_err());
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
