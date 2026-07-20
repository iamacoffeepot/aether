use super::*;

const FS_MAILBOX: &str = "aether.fs";
const FS_NAMESPACE_SAVE: &str = "save";

/// `aether.fs.write` followed by `aether.fs.read` round-trips the
/// bytes through the local-file adapter (ADR-0041). Both replies
/// echo the originating namespace + path for correlation; the read
/// reply also carries the bytes verbatim.
#[test]
fn fs_write_then_read_round_trips_in_save_namespace() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("substrate-bench-fs");
    let mut bench = SubstrateBench::builder()
        .full()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let path = "fs-roundtrip.bin".to_owned();
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];

    let result = bench
        .execute(vec![
            (
                "write",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &Write { namespace: FS_NAMESPACE_SAVE.to_owned(), path: path.clone(), bytes: payload.clone() },
                ),
            ),
            (
                "read",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &Read { namespace: FS_NAMESPACE_SAVE.to_owned(), path: path.clone() },
                ),
            ),
        ])
        .expect("write + read");

    match result.reply::<WriteResult>("write").expect("decode WriteResult") {
        WriteResult::Ok { namespace, path: echoed_path } => {
            assert_eq!(namespace, FS_NAMESPACE_SAVE);
            assert_eq!(echoed_path, path);
        }
        WriteResult::Err { error, .. } => panic!("write failed: {error:?}"),
    }
    match result.reply::<ReadResult>("read").expect("decode ReadResult") {
        ReadResult::Ok { namespace, path: echoed_path, bytes } => {
            assert_eq!(namespace, FS_NAMESPACE_SAVE);
            assert_eq!(echoed_path, path);
            assert_eq!(bytes, payload);
        }
        ReadResult::Err { error, .. } => panic!("read failed: {error:?}"),
    }
}

/// `aether.fs.delete` removes a previously-written file; a
/// follow-up `aether.fs.read` of the same path returns
/// `Err { NotFound }`.
#[test]
fn fs_delete_removes_written_file() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("substrate-bench-fs");
    let mut bench = SubstrateBench::builder()
        .full()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let path = "fs-delete.bin".to_owned();
    // A failed write would abort the sequence with `OpFailed`, so
    // reaching the asserts below means the write succeeded.
    let result = bench
        .execute(vec![
            (
                "write",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &Write { namespace: FS_NAMESPACE_SAVE.to_owned(), path: path.clone(), bytes: vec![1, 2, 3] },
                ),
            ),
            (
                "delete",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &Delete { namespace: FS_NAMESPACE_SAVE.to_owned(), path: path.clone() },
                ),
            ),
            ("read", BenchOp::send_and_await(FS_MAILBOX, &Read { namespace: FS_NAMESPACE_SAVE.to_owned(), path })),
        ])
        .expect("write + delete + read");

    match result.reply::<DeleteResult>("delete").expect("decode DeleteResult") {
        DeleteResult::Ok { .. } => {}
        DeleteResult::Err { error, .. } => panic!("delete failed: {error:?}"),
    }
    match result.reply::<ReadResult>("read").expect("decode ReadResult") {
        ReadResult::Ok { .. } => panic!("read should not have found a deleted file"),
        ReadResult::Err { error: FsError::NotFound, .. } => {}
        ReadResult::Err { error, .. } => panic!("expected NotFound, got {error:?}"),
    }
}

/// `aether.fs.list` enumerates entries under a prefix. After a
/// write to `<sandbox>/probe-list.bin`, listing the empty prefix
/// in `save` returns an entry list containing the bare filename.
#[test]
fn fs_list_returns_written_path() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("substrate-bench-fs");
    let mut bench = SubstrateBench::builder()
        .full()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let path = "probe-list.bin".to_owned();
    let result = bench
        .execute(vec![
            (
                "write",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &Write { namespace: FS_NAMESPACE_SAVE.to_owned(), path: path.clone(), bytes: vec![0] },
                ),
            ),
            (
                "list",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &List { namespace: FS_NAMESPACE_SAVE.to_owned(), prefix: String::new() },
                ),
            ),
        ])
        .expect("write + list");

    match result.reply::<ListResult>("list").expect("decode ListResult") {
        ListResult::Ok { entries, .. } => {
            assert!(entries.iter().any(|e| e == &path), "expected entries to include {path:?}; got {entries:?}");
        }
        ListResult::Err { error, .. } => panic!("list failed: {error:?}"),
    }
}

/// Reading a path that was never written returns
/// `Err { NotFound }`. Negative companion to the round-trip test.
#[test]
fn fs_read_unknown_path_returns_not_found() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("substrate-bench-fs");
    let mut bench = SubstrateBench::builder()
        .full()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let result = bench
        .execute(vec![(
            "read",
            BenchOp::send_and_await(
                FS_MAILBOX,
                &Read { namespace: FS_NAMESPACE_SAVE.to_owned(), path: "nonexistent-do-not-create.bin".to_owned() },
            ),
        )])
        .expect("read");

    match result.reply::<ReadResult>("read").expect("decode ReadResult") {
        ReadResult::Ok { .. } => panic!("read should not have found a never-written file"),
        ReadResult::Err { error: FsError::NotFound, .. } => {}
        ReadResult::Err { error, .. } => panic!("expected NotFound, got {error:?}"),
    }
}
