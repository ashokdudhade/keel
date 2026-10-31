//! Content hashing seam: one function over `sha2` + `hex`, so the digest
//! algorithm lives in exactly one place.
//!
//! Content hashes are stored in the index (`files.content_hash`) and wiki
//! manifests, so changing this function changes persisted bytes — keep it
//! stable, and bump the index format if it ever changes.

use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 over `data`.
pub(crate) fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(data.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors_are_stable() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
