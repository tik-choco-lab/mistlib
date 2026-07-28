#[path = "../src/session_registry.rs"]
mod session_registry;

use session_registry::SessionRegistry;

#[test]
fn join_a_and_b_coexist() {
    let mut reg = SessionRegistry::new();
    reg.insert("a".to_string(), "session-a");
    reg.insert("b".to_string(), "session-b");

    assert_eq!(reg.len(), 2);
    assert!(reg.contains("a"));
    assert!(reg.contains("b"));
    assert_eq!(
        reg.iter_in_join_order()
            .map(|(id, v)| (id.clone(), *v))
            .collect::<Vec<_>>(),
        vec![
            ("a".to_string(), "session-a"),
            ("b".to_string(), "session-b")
        ]
    );
}

#[test]
fn leave_room_id_keeps_the_other_room() {
    let mut reg = SessionRegistry::new();
    reg.insert("a".to_string(), "session-a");
    reg.insert("b".to_string(), "session-b");

    assert_eq!(reg.remove("a"), Some("session-a"));
    assert!(!reg.contains("a"));
    assert!(reg.contains("b"));
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.first(), Some((&"b".to_string(), &"session-b")));
}

#[test]
fn leave_room_clears_every_session() {
    let mut reg = SessionRegistry::new();
    reg.insert("a".to_string(), "session-a");
    reg.insert("b".to_string(), "session-b");

    let drained = reg.drain_all();

    assert_eq!(
        drained,
        vec![
            ("a".to_string(), "session-a"),
            ("b".to_string(), "session-b")
        ]
    );
    assert!(reg.is_empty());
    assert_eq!(reg.first(), None);
}

#[test]
fn double_join_keeps_a_single_session() {
    let mut reg = SessionRegistry::new();
    reg.insert("a".to_string(), "first");
    let previous = reg.insert("a".to_string(), "second");

    assert_eq!(previous, Some("first"));
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.get("a"), Some(&"second"));
    assert_eq!(
        reg.iter_in_join_order()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
        vec!["a".to_string()]
    );
}
