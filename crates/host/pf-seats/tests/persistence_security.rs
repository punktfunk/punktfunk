use pf_seats::persistence::LedgerStore;
#[cfg(unix)]
use pf_seats::persistence::LEDGER_FILE;

#[cfg(unix)]
#[test]
fn a_planted_hard_link_is_never_read_or_rewritten() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("seats");
    let store = LedgerStore::open(&root).unwrap();
    let outside = parent.path().join("outside");
    std::fs::write(&outside, b"attacker controlled").unwrap();
    std::fs::hard_link(&outside, root.join(LEDGER_FILE)).unwrap();

    assert!(store.load().is_err());
    assert_eq!(std::fs::read(outside).unwrap(), b"attacker controlled");
}

#[test]
fn secret_leaf_names_cannot_escape_the_root() {
    let temp = tempfile::tempdir().unwrap();
    let store = LedgerStore::open(temp.path()).unwrap();
    assert!(store.root().write_atomic("../outside", b"secret").is_err());
    assert!(store.root().write_atomic("stream:ads", b"secret").is_err());
    assert!(LedgerStore::open(temp.path().join("child/../escape")).is_err());
}

#[test]
fn nested_secret_roots_remove_only_safe_children() {
    let temp = tempfile::tempdir().unwrap();
    let store = LedgerStore::open(temp.path()).unwrap();
    let child = store.root().open_child_dir("hosts").unwrap();
    child.write_atomic("credential.bin", b"ciphertext").unwrap();
    child.remove_file("credential.bin").unwrap();
    child.remove_file("credential.bin").unwrap();
    assert!(
        !child.path().join("credential.bin").exists(),
        "child is gone"
    );
}
