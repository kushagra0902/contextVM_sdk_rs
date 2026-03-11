//! Encryption and gift wrapping for ContextVM.
//!
//! Provides NIP-44 encryption/decryption and NIP-59 gift wrapping.
//! The actual gift wrapping is done via nostr-sdk's Client for full NIP-59 compliance.

use crate::core::error::{Error, Result};
use nostr_sdk::prelude::*;

/// Encrypt a message using NIP-44.
pub async fn encrypt_nip44<T>(
    signer: &T,
    receiver_pubkey: &PublicKey,
    plaintext: &str,
) -> Result<String>
where
    T: NostrSigner,
{
    signer
        .nip44_encrypt(receiver_pubkey, plaintext)
        .await
        .map_err(|e| Error::Encryption(e.to_string()))
}

/// Decrypt a message using NIP-44.
pub async fn decrypt_nip44<T>(
    signer: &T,
    sender_pubkey: &PublicKey,
    ciphertext: &str,
) -> Result<String>
where
    T: NostrSigner,
{
    signer
        .nip44_decrypt(sender_pubkey, ciphertext)
        .await
        .map_err(|e| Error::Decryption(e.to_string()))
}

/// Decrypt a NIP-59 gift-wrapped event using the Client.
///
/// Returns the decrypted content string from the inner rumor event.
pub async fn decrypt_gift_wrap(client: &Client, event: &Event) -> Result<UnsignedEvent> {
    let unwrapped = client
        .unwrap_gift_wrap(event)
        .await
        .map_err(|e| Error::Decryption(e.to_string()))?;
    Ok(unwrapped.rumor)
}

/// Create and publish a NIP-59 gift-wrapped event.
///
/// Wraps an unsigned event (rumor) in a gift wrap addressed to the recipient.
pub async fn gift_wrap(
    client: &Client,
    recipient: &PublicKey,
    rumor: UnsignedEvent,
) -> Result<EventId> {
    let output = client
        .gift_wrap(recipient, rumor, Vec::<Tag>::new())
        .await
        .map_err(|e| Error::Encryption(e.to_string()))?;
    Ok(output.val)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_nip44_roundtrip() {
        let keys1 = Keys::generate();
        let keys2 = Keys::generate();

        let plaintext = "Hello, ContextVM!";

        let ciphertext = encrypt_nip44(&keys1, &keys2.public_key(), plaintext)
            .await
            .unwrap();

        let decrypted = decrypt_nip44(&keys2, &keys1.public_key(), &ciphertext)
            .await
            .unwrap();

        assert_eq!(plaintext, decrypted);
    }
}
