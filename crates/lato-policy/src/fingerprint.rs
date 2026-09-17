use lato_core::{ApprovalFingerprint, PolicyRequest};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

// v2: PolicyRequest gained plan_mode and tool_layer; the domain bump keeps
// v1-issued grants from ever matching a v2 request fingerprint.
const APPROVAL_FINGERPRINT_DOMAIN: &[u8] = b"lato.policy.approval.v2\0";

#[derive(Debug, thiserror::Error)]
pub enum FingerprintError {
    #[error("could not serialize policy fingerprint input: {0}")]
    Serialize(#[source] serde_json::Error),
}

pub fn canonical_arguments(value: &Value) -> Result<Vec<u8>, FingerprintError> {
    serde_json::to_vec(&normalize(value)).map_err(FingerprintError::Serialize)
}

pub fn approval_fingerprint(
    request: &PolicyRequest,
) -> Result<ApprovalFingerprint, FingerprintError> {
    let mut value = serde_json::to_value(request).map_err(FingerprintError::Serialize)?;
    sort_and_deduplicate_capabilities(&mut value);
    let canonical = canonical_arguments(&value)?;

    let mut hasher = Sha256::new();
    hasher.update(APPROVAL_FINGERPRINT_DOMAIN);
    hasher.update(canonical);
    let digest = hasher.finalize();

    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(ApprovalFingerprint(encoded))
}

fn normalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut normalized = Map::with_capacity(map.len());
            for key in keys {
                normalized.insert(key.clone(), normalize(&map[key]));
            }
            Value::Object(normalized)
        }
        Value::Array(values) => Value::Array(values.iter().map(normalize).collect()),
        other => other.clone(),
    }
}

fn sort_and_deduplicate_capabilities(value: &mut Value) {
    let Some(capabilities) = value.get_mut("capabilities").and_then(Value::as_array_mut) else {
        return;
    };
    capabilities.sort_unstable_by(|left, right| capability_key(left).cmp(capability_key(right)));
    capabilities.dedup();
}

fn capability_key(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
