#![allow(clippy::expect_used)]

use distill::{
    ARTIFACT_SCHEMA_VERSION, AcquisitionReceipt, ArtifactRef, ByteSpan, CL100K_PROFILE, CountUnit,
    Fidelity, MAX_ARTIFACT_LINEAGE_BYTES, MAX_LINEAGE_BYTES, POLICY_VERSION, PROJECTION_VERSION,
    PreservationResult, ProcessReceipt, ProcessStream, RECEIPT_SCHEMA_VERSION, Receipt,
    SourceVariant, StreamEvent,
};

const PROFILES: [&str; 12] = [
    "plain-text/v1",
    "build-log/v1",
    "test-log/v1",
    "diff/v1",
    "diagnostic/v1",
    "stack-trace/v1",
    "source-code/v1",
    "json/v1",
    "unicode/v1",
    "binary/v1",
    "untrusted-text/v1",
    "none/v1",
];

#[test]
fn deterministic_receipt_generator_freezes_lineage_budgets() {
    let spans = (0..256)
        .map(|index| ByteSpan {
            start: index,
            end: index + 1,
        })
        .collect::<Vec<_>>();
    let mut measured = Vec::new();

    for fidelity in [
        Fidelity::Exact,
        Fidelity::Extractive,
        Fidelity::Encoded,
        Fidelity::MetadataOnly,
    ] {
        for (count_unit, token_profile) in [
            (CountUnit::Bytes, None),
            (CountUnit::Tokens, Some(CL100K_PROFILE.to_owned())),
        ] {
            for span_count in [0, 256] {
                for profile in PROFILES {
                    let source_bytes = span_count as u64;
                    let artifact = ArtifactRef {
                        schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
                        id: "a".repeat(32),
                        source_sha256: "b".repeat(64),
                        source_bytes,
                        created_at: u64::MAX - 1,
                        expires_at: u64::MAX,
                    };
                    let root_id = "r".repeat(128);
                    let acquisition = if span_count == 0 {
                        AcquisitionReceipt {
                            variant: SourceVariant::Inline,
                            complete: true,
                            partial: false,
                            truncated: false,
                            root_id: None,
                            relative_path: None,
                            process: None,
                        }
                    } else {
                        AcquisitionReceipt {
                            variant: SourceVariant::Process,
                            complete: true,
                            partial: false,
                            truncated: false,
                            root_id: Some(root_id.clone()),
                            relative_path: None,
                            process: Some(ProcessReceipt {
                                events: spans
                                    .iter()
                                    .enumerate()
                                    .map(|(order, span)| StreamEvent {
                                        order: order as u64,
                                        stream: if order % 2 == 0 {
                                            ProcessStream::Stdout
                                        } else {
                                            ProcessStream::Stderr
                                        },
                                        span: *span,
                                    })
                                    .collect(),
                                exit_code: Some(i32::MAX),
                                signal: None,
                                timed_out: false,
                                working_directory: format!("{root_id}:<4096 path bytes>"),
                            }),
                        }
                    };
                    let (retained_spans, omitted_spans) = match fidelity {
                        Fidelity::Exact | Fidelity::Extractive => {
                            (spans[..span_count].to_vec(), Vec::new())
                        }
                        Fidelity::Encoded | Fidelity::MetadataOnly => {
                            (Vec::new(), spans[..span_count].to_vec())
                        }
                    };
                    let receipt = Receipt {
                        schema_version: RECEIPT_SCHEMA_VERSION.to_owned(),
                        request_id: "r".repeat(128),
                        source_sha256: artifact.source_sha256.clone(),
                        artifact: artifact.clone(),
                        projection_version: PROJECTION_VERSION.to_owned(),
                        policy_version: POLICY_VERSION.to_owned(),
                        token_profile: token_profile.clone(),
                        original_count: u64::MAX,
                        visible_count: u64::MAX,
                        count_unit,
                        fidelity,
                        retained_spans,
                        omitted_spans,
                        preservation: PreservationResult {
                            profile: profile.to_owned(),
                            mandatory_fact_ids: (0..256)
                                .map(|index| format!("fact-{index:03}"))
                                .collect(),
                        },
                        acquisition: acquisition.clone(),
                    };
                    measured
                        .push(serde_json::to_vec(&receipt).expect("compact receipt").len() as u64);
                }
            }
        }
    }

    measured.sort_unstable();
    assert_eq!(measured.len(), 4 * 2 * 2 * PROFILES.len());
    let maximum = *measured.last().expect("receipt size");
    let median = measured[(measured.len() - 1) / 2];
    eprintln!("generated receipt bytes: median={median} max={maximum}");
    assert!(maximum <= MAX_ARTIFACT_LINEAGE_BYTES);
    assert!(MAX_ARTIFACT_LINEAGE_BYTES / median >= 64);
    assert!(MAX_LINEAGE_BYTES / median >= 4_096);
}
