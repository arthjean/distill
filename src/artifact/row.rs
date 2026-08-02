pub(super) struct CommitReadbackRow {
    pub digest: String,
    pub bytes: Vec<u8>,
    pub acquisition_json: Vec<u8>,
    pub acquisition_digest: String,
    pub state: String,
}

pub(super) struct ArtifactReferenceRow {
    pub schema: String,
    pub digest: String,
    pub source_bytes: i64,
    pub created_at: i64,
    pub expires_at: i64,
}

pub(super) struct ReceiptTargetRow {
    pub schema: String,
    pub digest: String,
    pub source_bytes: i64,
    pub created_at: i64,
    pub expires_at: i64,
    pub lineage_count: i64,
    pub lineage_bytes: i64,
    pub lineage_head: String,
}

pub(super) struct LineageClaimRow {
    pub count: i64,
    pub bytes: i64,
    pub head: String,
}

pub(super) struct ReceiptLineageRow {
    pub sequence: i64,
    pub request_id: String,
    pub receipt_json: Vec<u8>,
    pub receipt_digest: String,
    pub chain: String,
}

pub(super) struct StatusRow {
    pub records: i64,
    pub bytes: i64,
    pub expired: i64,
}

pub(super) struct ArtifactRow {
    pub schema: String,
    pub state: String,
    pub bytes: Vec<u8>,
    pub digest: String,
    pub source_bytes: i64,
    pub acquisition_json: Vec<u8>,
    pub acquisition_digest: String,
    pub created_at: i64,
    pub expires_at: i64,
}

pub(super) struct LineageShapeRow {
    pub count: i64,
    pub bytes: i64,
    pub minimum: i64,
    pub maximum: i64,
    pub distinct: i64,
}

pub(super) struct GarbageCollectionRow {
    pub records: i64,
    pub bytes: i64,
}

pub(super) struct LegacyReceiptRow {
    pub sequence: i64,
    pub artifact_id: String,
    pub request_id: String,
    pub receipt_json: Vec<u8>,
}
