use relay::{Error, MAX_PAYLOAD_BYTES, MAX_RESULT_BYTES, State, Store};
use std::sync::{Arc, Barrier};

#[test]
fn round_trip_survives_reopening_and_idempotent_retries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.sqlite");
    let mut store = Store::open(&path).unwrap();
    let task = store.submit("k", "opaque request").unwrap();
    assert_eq!(task, store.submit("k", "opaque request").unwrap());
    assert!(matches!(
        store.submit("k", "changed"),
        Err(Error::IdempotencyConflict)
    ));
    drop(store);
    let mut store = Store::open(&path).unwrap();
    let claimed = store.claim_next("worker").unwrap().unwrap();
    assert_eq!(claimed.id, task.id);
    let claim = claimed.claim().unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert!(store.claim_next("other").unwrap().is_none());
    assert_eq!(store.active_claim().unwrap(), Some(claimed));
    let finished = store.finish(&claim, "bounded result").unwrap();
    assert_eq!(finished, store.finish(&claim, "bounded result").unwrap());
    assert!(matches!(
        store.finish(&claim, "changed"),
        Err(Error::StaleClaim)
    ));
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.get(task.id).unwrap(), finished);
    assert_eq!(finished.state, State::Finished);
}

#[test]
fn recovery_is_explicit_and_fences_old_claims_even_for_same_owner() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("relay.sqlite")).unwrap();
    store.submit("first", "a").unwrap();
    store.submit("second", "b").unwrap();
    let old = store
        .claim_next("worker")
        .unwrap()
        .unwrap()
        .claim()
        .unwrap();
    assert!(store.claim_next("worker2").unwrap().is_none());
    let mut wrong = old.clone();
    wrong.owner = "imposter".into();
    assert!(matches!(store.finish(&wrong, "x"), Err(Error::StaleClaim)));
    assert!(matches!(
        store.confirm_stopped_and_requeue(&wrong),
        Err(Error::StaleClaim)
    ));
    store.confirm_stopped_and_requeue(&old).unwrap();
    assert!(matches!(store.finish(&old, "x"), Err(Error::StaleClaim)));
    let new = store
        .claim_next("worker")
        .unwrap()
        .unwrap()
        .claim()
        .unwrap();
    assert_eq!(new.generation, old.generation + 1);
    assert!(matches!(store.finish(&old, "x"), Err(Error::StaleClaim)));
    assert!(matches!(
        store.confirm_stopped_and_requeue(&old),
        Err(Error::StaleClaim)
    ));
    store.finish(&new, "done").unwrap();
    assert_ne!(store.claim_next("worker").unwrap().unwrap().id, old.task_id);
}

#[test]
fn bounds_are_utf8_bytes_and_rejection_preserves_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("relay.sqlite")).unwrap();
    assert!(store.submit("", "x").is_err());
    assert!(store.submit(&"é".repeat(65), "x").is_err());
    assert!(
        store
            .submit("oversize", &"x".repeat(MAX_PAYLOAD_BYTES + 1))
            .is_err()
    );
    let task = store
        .submit("limit", &"x".repeat(MAX_PAYLOAD_BYTES))
        .unwrap();
    assert!(store.claim_next("").is_err());
    let claim = store
        .claim_next("worker")
        .unwrap()
        .unwrap()
        .claim()
        .unwrap();
    assert!(
        store
            .finish(&claim, &"x".repeat(MAX_RESULT_BYTES + 1))
            .is_err()
    );
    assert_eq!(store.get(task.id).unwrap().state, State::Claimed);
    store.finish(&claim, &"x".repeat(MAX_RESULT_BYTES)).unwrap();
}

#[test]
fn independent_connections_only_allow_one_global_claim() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.sqlite");
    let mut store = Store::open(&path).unwrap();
    for n in 0..8 {
        store.submit(&n.to_string(), "request").unwrap();
    }
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|n| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(path).unwrap();
                barrier.wait();
                store.claim_next(&n.to_string()).unwrap().is_some()
            })
        })
        .collect();
    let count = handles
        .into_iter()
        .map(|h| usize::from(h.join().unwrap()))
        .sum::<usize>();
    assert_eq!(count, 1);
}

#[test]
fn concurrent_duplicate_submissions_share_one_task() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.sqlite");
    Store::open(&path).unwrap();
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(path).unwrap();
                barrier.wait();
                store.submit("same", "same payload").unwrap().id
            })
        })
        .collect();
    let ids: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(ids.iter().all(|id| *id == ids[0]));
}
