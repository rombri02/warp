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

/// A session whose shell has already exited, so `kill` never signals anything.
fn exited_session() -> Arc<Session> {
    Arc::new(Session {
        pid: u32::MAX,
        exited: AtomicBool::new(true),
        uuid: Some("pane".to_owned()),
        shell_session_id: SessionId::from(7),
        leader: File::open("/dev/null").unwrap(),
        output: Mutex::new(Output {
            log: OutputLog::default(),
            attachment: None,
            detached_since: None,
        }),
    })
}

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn reattaching_with_nothing_recorded_does_not_mute_input() {
    let session = exited_session();
    let (_client, attach) = session.attach(true).unwrap();
    let attach = attach.expect("reattaching should describe the session");
    assert_eq!(attach.replay_len, 0);
    assert!(attach.unmute_token.is_empty());
    assert!(!session.output.lock().attachment.as_ref().unwrap().replaying);
}

#[test]
fn reattaching_replays_recording_then_unmutes() {
    let session = exited_session();
    session.output.lock().log.push(b"recorded");
    let (mut client, attach) = session.attach(true).unwrap();
    let attach = attach.unwrap();
    assert_eq!(attach.replay_len, b"recorded".len());
    assert!(!attach.unmute_token.is_empty());

    let mut replayed = vec![0; attach.replay_len];
    client.read_exact(&mut replayed).unwrap();
    assert_eq!(replayed, b"recorded");
    assert!(wait_until(|| {
        !session.output.lock().attachment.as_ref().unwrap().replaying
    }));
}

#[test]
fn detaching_client_starts_orphan_clock() {
    let session = exited_session();
    let (client, _) = session.attach(false).unwrap();
    assert!(session.output.lock().detached_since.is_none());
    drop(client);
    assert!(wait_until(|| session
        .output
        .lock()
        .detached_since
        .is_some()));
    assert!(session.output.lock().attachment.is_none());
}

#[test]
fn only_one_daemon_holds_the_instance_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.lock");
    let first = acquire_instance_lock(&path);
    assert!(first.is_some());
    assert!(acquire_instance_lock(&path).is_none());
    drop(first);
    assert!(acquire_instance_lock(&path).is_some());
}
