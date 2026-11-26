//! `rustyschwab-admin repair-order-ids`, run as an operator runs it.
use rustyschwab::{execution::OrderJournal, utils::mask_account};
use serde_json::json;
use std::{
    path::Path,
    process::{Command, Output},
};

fn admin(state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rustyschwab-admin"))
        .arg("--state")
        .arg(state)
        .args(args)
        .output()
        .unwrap()
}

fn succeeded(output: &Output) -> Vec<String> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Every row that names an order, the audit trail and the metadata, as text.
fn contents(state: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(state).unwrap();
    [
        "SELECT account||' '||order_id||' '||observed_at||' '||payload FROM orders ORDER BY 1",
        "SELECT execution_id FROM executions ORDER BY 1",
        "SELECT id||' '||coalesce(outcome,'') FROM submissions ORDER BY 1",
        "SELECT id||' '||coalesce(replaces,'') FROM reservations ORDER BY 1",
        "SELECT sequence||' '||kind||' '||subject||' '||hash FROM audit ORDER BY sequence",
        "SELECT key||' '||value FROM meta ORDER BY 1",
    ]
    .iter()
    .flat_map(|query| {
        let mut statement = connection.prepare(query).unwrap();
        statement
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
    })
    .collect()
}

/// A state file someone edited past its migration ("007", "+8" and "abc"
/// written straight into it) does not open. `repair-order-ids --dry-run`
/// lists each change it would make and changes nothing. Run without it, in
/// an operator's name, it makes them, audited as the migration audits
/// them, and the file opens. Run again, it finds nothing to do.
#[tokio::test]
async fn repair_order_ids_lists_then_repairs_a_state_file_that_fails_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let journal = OrderJournal::new(dir.path().join("orders.json"));
    journal.records().await.unwrap();
    let state = journal.path().to_owned();
    let written = rusqlite::Connection::open(&state).unwrap();
    for order_id in ["007", "+8", "abc"] {
        let working = json!({"orderId": order_id, "status": "WORKING"}).to_string();
        written
            .execute(
                "INSERT INTO orders VALUES('A',?1,0,?2)",
                rusqlite::params![order_id, working],
            )
            .unwrap();
    }
    drop(written);
    let opens = || async { OrderJournal::new(state.clone()).records().await.map(drop) };
    assert!(matches!(
        opens().await,
        Err(rustyschwab::Error::NonCanonicalOrderId { .. })
    ));
    let row = format!("orders (account {})", mask_account("A"));
    let changes = vec![
        format!("{row}: \"+8\" -> \"8\""),
        format!("{row}: \"007\" -> \"7\""),
        format!("{row}: \"abc\" -> set aside"),
    ];

    let before = contents(&state);
    assert_eq!(
        succeeded(&admin(&state, &["repair-order-ids", "--dry-run"])),
        changes
    );
    assert_eq!(contents(&state), before);
    assert!(opens().await.is_err());
    // A dry run of the command about to be run changes nothing either.
    assert_eq!(
        succeeded(&admin(
            &state,
            &["repair-order-ids", "--dry-run", "--operator", "alice"]
        )),
        changes
    );
    assert_eq!(contents(&state), before);
    // A repair names the operator who made it.
    assert!(!admin(&state, &["repair-order-ids"]).status.success());
    assert_eq!(contents(&state), before);

    assert_eq!(
        succeeded(&admin(&state, &["repair-order-ids", "--operator", "alice"])),
        changes
    );
    opens().await.unwrap();
    let repaired = OrderJournal::new(state.clone());
    let audited: Vec<(String, String)> = repaired
        .audit_page(0, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.starts_with("order_id"))
        .map(|e| (e.kind, e.subject))
        .collect();
    let entry = |kind: &str, subject: &str| (kind.to_string(), subject.to_string());
    assert_eq!(
        audited,
        vec![
            entry("order_id_canonicalized", "+8"),
            entry("order_id_canonicalized", "007"),
            entry("order_id_set_aside", "abc"),
            entry("order_ids_repaired", "alice"),
        ]
    );
    repaired.verify_audit().await.unwrap();

    let after = contents(&state);
    assert!(succeeded(&admin(&state, &["repair-order-ids", "--operator", "alice"])).is_empty());
    assert_eq!(contents(&state), after);
}
