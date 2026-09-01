use lato_core::{ApprovalFingerprint, SandboxObligation};
use lato_policy::{ApprovalError, ApprovalLedger};
use std::{
    sync::{Arc, Barrier},
    thread,
    time::Duration,
};

fn fingerprint(value: &str) -> ApprovalFingerprint {
    ApprovalFingerprint(value.into())
}

#[test]
fn expired_grant_is_rejected_and_cannot_be_retried() {
    let ledger = ApprovalLedger::new(Duration::ZERO);
    let expected = fingerprint("expired");
    let grant = ledger
        .issue(expected.clone(), SandboxObligation::workspace("/workspace"))
        .unwrap();

    assert_eq!(
        ledger.consume(&grant, &expected),
        Err(ApprovalError::Expired)
    );
    assert_eq!(
        ledger.consume(&grant, &expected),
        Err(ApprovalError::ConsumedOrMissing)
    );
}

#[test]
fn consumed_grant_cannot_be_replayed() {
    let ledger = ApprovalLedger::new(Duration::from_secs(60));
    let expected = fingerprint("single-use");
    let grant = ledger
        .issue(expected.clone(), SandboxObligation::workspace("/workspace"))
        .unwrap();

    assert_eq!(ledger.consume(&grant, &expected), Ok(()));
    assert_eq!(
        ledger.consume(&grant, &expected),
        Err(ApprovalError::ConsumedOrMissing)
    );
}

#[test]
fn mismatch_consumes_the_grant_and_cannot_be_corrected() {
    let ledger = ApprovalLedger::new(Duration::from_secs(60));
    let issued = fingerprint("issued");
    let grant = ledger
        .issue(issued.clone(), SandboxObligation::workspace("/workspace"))
        .unwrap();

    assert_eq!(
        ledger.consume(&grant, &fingerprint("different")),
        Err(ApprovalError::Mismatch)
    );
    assert_eq!(
        ledger.consume(&grant, &issued),
        Err(ApprovalError::ConsumedOrMissing)
    );
}

#[test]
fn tampered_grant_fields_are_rejected_and_consumed() {
    let ledger = ApprovalLedger::new(Duration::from_secs(60));
    let expected = fingerprint("exact");
    let grant = ledger
        .issue(expected.clone(), SandboxObligation::workspace("/workspace"))
        .unwrap();

    let mut changed_fingerprint = grant.clone();
    changed_fingerprint.fingerprint = fingerprint("forged");
    assert_eq!(
        ledger.consume(&changed_fingerprint, &expected),
        Err(ApprovalError::Mismatch)
    );
    assert_eq!(
        ledger.consume(&grant, &expected),
        Err(ApprovalError::ConsumedOrMissing)
    );

    let grant = ledger
        .issue(expected.clone(), SandboxObligation::workspace("/workspace"))
        .unwrap();
    let mut changed_sandbox = grant.clone();
    changed_sandbox.sandbox = SandboxObligation::off("/workspace");
    assert_eq!(
        ledger.consume(&changed_sandbox, &expected),
        Err(ApprovalError::Mismatch)
    );
    assert_eq!(
        ledger.consume(&grant, &expected),
        Err(ApprovalError::ConsumedOrMissing)
    );
}

#[test]
fn eight_concurrent_consumers_have_exactly_one_success() {
    let ledger = Arc::new(ApprovalLedger::new(Duration::from_secs(60)));
    let expected = fingerprint("concurrent");
    let grant = Arc::new(
        ledger
            .issue(expected.clone(), SandboxObligation::workspace("/workspace"))
            .unwrap(),
    );
    let barrier = Arc::new(Barrier::new(8));

    let handles = (0..8)
        .map(|_| {
            let ledger = Arc::clone(&ledger);
            let grant = Arc::clone(&grant);
            let expected = expected.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                ledger.consume(&grant, &expected)
            })
        })
        .collect::<Vec<_>>();

    let successes = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(Result::is_ok)
        .count();
    assert_eq!(successes, 1);
}

#[test]
fn approval_errors_have_stable_codes() {
    assert_eq!(ApprovalError::Expired.code(), "policy.grant_expired");
    assert_eq!(ApprovalError::Mismatch.code(), "policy.grant_mismatch");
    assert_eq!(
        ApprovalError::ConsumedOrMissing.code(),
        "policy.grant_consumed"
    );
    assert_eq!(ApprovalError::Unavailable.code(), "policy.unavailable");
}
