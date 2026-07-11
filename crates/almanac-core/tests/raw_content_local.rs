//! Raw-content-local invariant (ARCHITECTURE.md §6.2), enforced at the type
//! level: every outbound network call in this codebase serializes request
//! data through serde (reqwest query/form/json builders). `RawContent` and
//! `SourceObject` implement neither `Serialize` nor `Deserialize`, so raw
//! source content CANNOT be placed into an outbound call (or any other serde
//! sink) without a loud, reviewable escape hatch. These are compile-time
//! assertions — if someone derives Serialize on these types, this test file
//! stops compiling.

use almanac_core::types::{ProvenanceRef, RawContent, SourceObject};
use static_assertions::assert_not_impl_any;

assert_not_impl_any!(RawContent: serde::Serialize, serde::de::DeserializeOwned);
assert_not_impl_any!(SourceObject: serde::Serialize, serde::de::DeserializeOwned);
// ProvenanceRef is grounding metadata, not raw content — but as of Phase 2 it
// has no serialization need either; keep it locked down until a later phase
// deliberately opens it.
assert_not_impl_any!(ProvenanceRef: serde::Serialize);

#[test]
fn raw_content_is_only_readable_locally() {
    let raw = RawContent::new(serde_json::json!({"snippet": "local only"}));
    // Local, deliberate read access for on-device processing is the only door.
    assert_eq!(raw.as_json()["snippet"], "local only");
}
