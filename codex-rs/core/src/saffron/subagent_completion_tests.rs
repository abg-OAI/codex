use super::*;

#[test]
fn sibling_turns_retain_shared_ancestors_until_both_finish() {
    let retention = Arc::new(AncestorTurnRetention::default());
    let root_thread_id = ThreadId::new();
    let parent_thread_id = ThreadId::new();

    let first = retention
        .retain([root_thread_id, parent_thread_id])
        .expect("first child should retain ancestors");
    let second = retention
        .retain([root_thread_id, parent_thread_id])
        .expect("second child should retain ancestors");

    drop(first);
    assert!(retention.is_retained(root_thread_id));
    assert!(retention.is_retained(parent_thread_id));

    drop(second);
    assert!(!retention.is_retained(root_thread_id));
    assert!(!retention.is_retained(parent_thread_id));
}

#[test]
fn nested_turns_release_only_their_own_ancestor_chain() {
    let retention = Arc::new(AncestorTurnRetention::default());
    let root_thread_id = ThreadId::new();
    let parent_thread_id = ThreadId::new();

    let child = retention
        .retain([root_thread_id])
        .expect("child should retain root");
    let grandchild = retention
        .retain([root_thread_id, parent_thread_id])
        .expect("grandchild should retain both ancestors");

    drop(child);
    assert!(retention.is_retained(root_thread_id));
    assert!(retention.is_retained(parent_thread_id));

    drop(grandchild);
    assert!(!retention.is_retained(root_thread_id));
    assert!(!retention.is_retained(parent_thread_id));
}

#[test]
fn duplicate_ancestor_ids_count_as_one_lease() {
    let retention = Arc::new(AncestorTurnRetention::default());
    let root_thread_id = ThreadId::new();

    let guard = retention
        .retain([root_thread_id, root_thread_id])
        .expect("ancestor should be retained");
    drop(guard);

    assert!(!retention.is_retained(root_thread_id));
}
