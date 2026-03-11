//! Nostr signer utilities.
//!
//! Re-exports key types from nostr-sdk and provides convenience constructors.

pub use nostr_sdk::prelude::{Keys, NostrSigner, PublicKey};

/// Create keys from a private key string (hex or nsec/bech32).
pub fn from_sk(sk: &str) -> std::result::Result<Keys, nostr_sdk::key::Error> {
    Keys::parse(sk)
}

/// Generate a new random keypair.
pub fn generate() -> Keys {
    Keys::generate()
}
