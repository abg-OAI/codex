use super::*;
use crate::RolloutRecorder;
use crate::WriterLockCoordinator;
use codex_protocol::ThreadId;
use std::sync::Arc;
use tempfile::TempDir;

fn create_source(home: &TempDir, id: ThreadId) -> io::Result<PathBuf> {
    let path = home
        .path()
        .join(format!("rollout-2026-11-17T18-00-00-{id}.jsonl"));
    fs::write(&path, b"original\n")?;
    Ok(path)
}

fn prepare_intent(path: &Path, id: ThreadId) -> io::Result<Vec<u8>> {
    let items = vec![
        RolloutItem::DynamicToolsUpdated {
            thread_id: id,
            tools: vec![]
        };
        4
    ];
    let next = prepare(
        path,
        &File::open(path)?,
        &items,
        RolloutOrdinalState::Paginated { next: Some(17) },
    )?;
    assert_eq!(next.current()?, Some(21));
    Ok(fs::read(intent_path(path))?[HEADER_LEN as usize..].to_vec())
}

#[tokio::test]
async fn replays_after_each_record_and_torn_json_without_duplicate_bytes() -> io::Result<()> {
    for record in 0..=4 {
        for torn in [false, true] {
            if record == 4 && torn {
                continue;
            }
            let home = TempDir::new()?;
            let id = ThreadId::new();
            let path = create_source(&home, id)?;
            let locks = Arc::new(WriterLockCoordinator::new(home.path()));
            let bytes = prepare_intent(&path, id)?;
            let lines: Vec<_> = bytes.split_inclusive(|&byte| byte == b'\n').collect();
            assert_eq!(lines.len(), 4);
            let complete: usize = lines[..record].iter().map(|line| line.len()).sum();
            let end = if torn {
                complete + lines[record].len() / 2
            } else {
                complete
            };
            File::options()
                .append(true)
                .open(&path)?
                .write_all(&bytes[..end])?;
            assert_eq!(
                RolloutRecorder::load_rollout_items(&path)
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(
                ensure_checkpoint_complete(&path.with_added_extension("zst"))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
            let lock = Arc::new(locks.acquire(id)?);
            assert!(RolloutRecorder::recover_checkpoint(&path, lock.clone()).await?);
            assert!(!RolloutRecorder::recover_checkpoint(&path, lock).await?);
            assert_eq!(
                fs::read(&path)?,
                [b"original\n".as_slice(), &bytes].concat()
            );
            ensure_checkpoint_complete(&path)?;
        }
    }
    Ok(())
}

#[test]
fn unpublished_stage_leaves_source_readable_and_is_removed_by_its_owner() -> io::Result<()> {
    let home = TempDir::new()?;
    let id = ThreadId::new();
    let path = create_source(&home, id)?;
    let locks = Arc::new(WriterLockCoordinator::new(home.path()));
    fs::write(staging_path(&path), b"unfinished private stage")?;
    ensure_checkpoint_complete(&path)?;
    assert_eq!(
        recover(&path, &locks.acquire(ThreadId::new())?)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(!recover(&path, &locks.acquire(id)?)?);
    assert!(!staging_path(&path).exists());
    assert_eq!(fs::read(&path)?, b"original\n");
    let compressed = crate::compression::compressed_rollout_path(&path);
    fs::rename(&path, &compressed)?;
    fs::write(staging_path(&path), b"unpublished stage")?;
    assert!(!recover(&compressed, &locks.acquire(id)?)?);
    assert!(!staging_path(&path).exists());
    Ok(())
}

#[test]
fn replay_refuses_changed_inode_divergent_bytes_and_truncated_intent() -> io::Result<()> {
    for damage in [
        "source replaced",
        "overlapping bytes changed",
        "truncated intent",
    ] {
        let home = TempDir::new()?;
        let id = ThreadId::new();
        let path = create_source(&home, id)?;
        let _batch = prepare_intent(&path, id)?;
        match damage {
            "source replaced" => {
                fs::rename(&path, home.path().join("old-inode"))?;
                fs::write(&path, b"original\n")?;
            }
            "overlapping bytes changed" => {
                File::options()
                    .append(true)
                    .open(&path)?
                    .write_all(b"wrong bytes")?;
            }
            _ => File::options()
                .write(true)
                .open(intent_path(&path))?
                .set_len(HEADER_LEN + 1)?,
        }
        let before = fs::read(&path)?;
        let locks = Arc::new(WriterLockCoordinator::new(home.path()));
        assert_eq!(
            recover(&path, &locks.acquire(id)?).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(&path)?, before);
        assert!(intent_path(&path).exists());
    }
    Ok(())
}
