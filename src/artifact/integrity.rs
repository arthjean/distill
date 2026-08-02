use crate::types::{ArtifactRef, Failure, FailureCode};
use sha2::{Digest, Sha256};

pub(super) const EMPTY_LINEAGE_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const LINEAGE_DIGEST_DOMAIN: &[u8] = b"distill.lineage/v1\0";

pub(super) fn nonnegative_u64(value: i64, label: &str) -> Result<u64, Failure> {
    u64::try_from(value).map_err(|_| {
        Failure::new(
            FailureCode::ArtifactCorrupt,
            format!("artifact {label} is invalid"),
        )
    })
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn verify_exact_digest(
    bytes: &[u8],
    digest: &str,
    message: &'static str,
    reference: &ArtifactRef,
) -> Result<(), Failure> {
    if !valid_sha256(digest) || sha256_hex(bytes) != digest {
        return Err(
            Failure::new(FailureCode::ArtifactCorrupt, message).with_artifact(reference.clone())
        );
    }
    Ok(())
}

pub(super) fn valid_sha256(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(crate::contract::is_lower_hex)
}

pub(super) fn lineage_chain_sha256(previous: &str, sequence: u64, receipt_json: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(LINEAGE_DIGEST_DOMAIN);
    digest.update(previous.as_bytes());
    digest.update(sequence.to_be_bytes());
    digest.update(receipt_json);
    format!("{:x}", digest.finalize())
}
