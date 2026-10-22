use remodex_connect_native::{
    auth::{Auth, Metadata, ACCESS_MS},
    store::Store,
};
use tempfile::TempDir;

fn auth() -> (TempDir, Auth) {
    let dir = tempfile::tempdir().unwrap();
    let auth = Auth::new(Store::open(dir.path()).unwrap());
    (dir, auth)
}
fn pair(
    a: &mut Auth,
    now: i64,
    proof: Option<&str>,
    installation: Option<&str>,
) -> (String, remodex_connect_native::auth::Exchange) {
    let invite = a.invite("test", "http://127.0.0.1:1", now).unwrap();
    let token = invite["pairingToken"].as_str().unwrap().to_owned();
    let result = a
        .exchange(
            &token,
            proof,
            Metadata {
                installation_id: installation.map(str::to_owned),
                ..Default::default()
            },
            now,
        )
        .unwrap()
        .unwrap();
    (token, result)
}
#[test]
fn token_is_single_use_with_proof_bound_retry_and_revocation() {
    let (_dir, mut a) = auth();
    let proof = "a".repeat(64);
    let (token, result) = pair(&mut a, 1000, Some(&proof), None);
    assert!(a
        .exchange(&token, None, Metadata::default(), 1001)
        .unwrap()
        .is_none());
    assert!(a
        .exchange(&token, Some(&"b".repeat(64)), Metadata::default(), 1001)
        .unwrap()
        .is_none());
    assert_eq!(
        a.exchange(&token, Some(&proof), Metadata::default(), 1001)
            .unwrap()
            .unwrap()
            .access_token,
        result.access_token
    );
    a.revoke(&result.client_id).unwrap();
    assert!(a
        .authenticate(&result.access_token, 1001)
        .unwrap()
        .is_none());
    assert!(a
        .exchange(&token, Some(&proof), Metadata::default(), 1001)
        .unwrap()
        .is_none());
}
#[test]
fn durable_tokens_survive_restart_without_persisting_secrets_or_tickets() {
    let (dir, mut a) = auth();
    let (_, result) = pair(&mut a, 1000, None, None);
    let ticket = a.ticket(&result.client_id, 1000).unwrap().unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_owned();
    let environment = a.store.environment_id().unwrap();
    drop(a);
    let mut a = Auth::new(Store::open(dir.path()).unwrap());
    assert_eq!(a.store.environment_id().unwrap(), environment);
    assert_eq!(
        a.authenticate(&result.access_token, 1001)
            .unwrap()
            .unwrap()
            .id,
        result.client_id
    );
    assert!(a.consume_ticket(&ticket, 1001).unwrap().is_none());
    assert!(a
        .authenticate(&result.access_token, 1000 + ACCESS_MS)
        .unwrap()
        .is_none());
    for file in std::fs::read_dir(dir.path()).unwrap() {
        let bytes = std::fs::read(file.unwrap().path()).unwrap();
        assert!(!bytes
            .windows(result.access_token.len())
            .any(|w| w == result.access_token.as_bytes()));
        assert!(!bytes.windows(ticket.len()).any(|w| w == ticket.as_bytes()));
    }
}
#[test]
fn re_pairing_installation_revokes_existing_token_and_tickets_atomically() {
    let (_dir, mut a) = auth();
    let (_, first) = pair(&mut a, 1000, None, Some("device-one"));
    let ticket = a.ticket(&first.client_id, 1000).unwrap().unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, second) = pair(&mut a, 1001, None, Some("device-one"));
    assert_eq!(second.replaced_client_ids, vec![first.client_id]);
    assert!(a.authenticate(&first.access_token, 1001).unwrap().is_none());
    assert!(a.consume_ticket(&ticket, 1001).unwrap().is_none());
    assert!(a
        .authenticate(&second.access_token, 1001)
        .unwrap()
        .is_some());
}
#[test]
fn tickets_are_single_use_expiring_and_capacity_is_bounded() {
    let (_dir, mut a) = auth();
    let (_, client) = pair(&mut a, 1000, None, None);
    let ticket = a.ticket(&client.client_id, 1000).unwrap().unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        a.consume_ticket(&ticket, 1001).unwrap().unwrap(),
        client.client_id
    );
    assert!(a.consume_ticket(&ticket, 1002).unwrap().is_none());
    let expired = a.ticket(&client.client_id, 1000).unwrap().unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(a.consume_ticket(&expired, 31000).unwrap().is_none());
    for _ in 0..128 {
        assert!(a.ticket(&client.client_id, 31000).unwrap().is_some());
    }
    assert!(a.ticket(&client.client_id, 31000).unwrap().is_none());
    assert!(a.ticket(&client.client_id, 61000).unwrap().is_some());
}
#[test]
fn expired_and_malformed_pairing_requests_never_issue_tokens() {
    let (_dir, mut a) = auth();
    let invite = a.invite("test", "http://127.0.0.1", 1000).unwrap();
    let raw = invite["pairingToken"].as_str().unwrap();
    for proof in ["short", &"Z".repeat(64), &" ".repeat(64)] {
        assert!(a
            .exchange(raw, Some(proof), Metadata::default(), 1000)
            .unwrap()
            .is_none());
    }
    assert!(a
        .exchange(raw, None, Metadata::default(), 301000)
        .unwrap()
        .is_none());
    for _ in 0..128 {
        a.invite("test", "http://127.0.0.1", 301000).unwrap();
    }
    assert!(a.invite("test", "http://127.0.0.1", 301000).is_err());
    assert!(a.invite("test", "http://127.0.0.1", 601000).is_ok());
}
#[test]
fn failed_database_commit_does_not_consume_invitation() {
    let (dir, mut a) = auth();
    let invite = a.invite("test", "http://127.0.0.1", 1000).unwrap();
    let raw = invite["pairingToken"].as_str().unwrap();
    let lock = rusqlite::Connection::open(dir.path().join("native-connect.sqlite")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(a.exchange(raw, None, Metadata::default(), 1000).is_err());
    lock.execute_batch("ROLLBACK").unwrap();
    assert!(a
        .exchange(raw, None, Metadata::default(), 1001)
        .unwrap()
        .is_some());
}
