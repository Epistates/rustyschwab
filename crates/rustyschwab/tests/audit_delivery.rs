use rustyschwab::{
    audit_delivery::{AuditAcknowledgement, AuditBatch, audit_digest},
    execution::OrderJournal,
};
use std::sync::{Arc, Mutex};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method},
};
#[tokio::test]
async fn wrong_ack_never_advances_cursor_and_retry_reuses_exact_batch() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("audit.json"));
    journal
        .begin("A", "place_order", serde_json::json!({"tag":"one"}))
        .await
        .unwrap();
    let server = MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let bodies = seen.clone();
    Mock::given(method("POST"))
        .and(header(
            "authorization",
            "Bearer synthetic-audit-credential-00000000",
        ))
        .respond_with(move |r: &wiremock::Request| {
            let batch: AuditBatch = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(audit_digest(&batch).unwrap(), batch.batch_sha256);
            let mut seen = bodies.lock().unwrap();
            seen.push(r.body.clone());
            ResponseTemplate::new(200).set_body_json(AuditAcknowledgement {
                stream_id: batch.stream_id,
                through_sequence: batch.events.last().unwrap().sequence,
                batch_sha256: if seen.len() == 1 {
                    "wrong".into()
                } else {
                    batch.batch_sha256
                },
            })
        })
        .expect(2)
        .mount(&server)
        .await;
    let token = || secrecy::SecretString::from("synthetic-audit-credential-00000000".to_owned());
    assert!(
        journal
            .deliver_audit("archive", &server.uri(), token(), 100)
            .await
            .is_err()
    );
    assert!(
        journal
            .deliver_audit("archive", &server.uri(), token(), 100)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        journal
            .deliver_audit("archive", &server.uri(), token(), 100)
            .await
            .unwrap()
            .is_none()
    );
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], seen[1]);
    }
    server.verify().await;
}
