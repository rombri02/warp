use super::*;

#[test]
fn output_log_keeps_head_and_recent_tail() {
    let mut log = OutputLog::default();
    let head = vec![b'h'; LOG_HEAD_CAPACITY];
    log.push(&head);
    log.push(&vec![b'x'; LOG_TAIL_CAPACITY]);
    log.push(b"recent");

    let snapshot = log.snapshot();
    assert_eq!(snapshot.len(), LOG_HEAD_CAPACITY + LOG_TAIL_CAPACITY);
    assert!(snapshot.starts_with(&head));
    assert!(snapshot.ends_with(b"recent"));
    assert_eq!(log.total, LOG_HEAD_CAPACITY + LOG_TAIL_CAPACITY + 6);
}

#[test]
fn output_log_since_returns_bytes_after_offset() {
    let mut log = OutputLog::default();
    log.push(b"before");
    let offset = log.total;
    log.push(b"after");
    assert_eq!(log.since(offset), b"after");

    let mut log = OutputLog::default();
    log.push(&vec![b'h'; LOG_HEAD_CAPACITY - 2]);
    let offset = log.total;
    log.push(b"abcd");
    assert_eq!(log.since(offset), b"abcd");
}

#[test]
fn find_locates_token_across_pushes() {
    assert_eq!(find(b"xxTOKENyy", b"TOKEN"), Some(2));
    assert_eq!(find(b"TOKE", b"TOKEN"), None);
}
