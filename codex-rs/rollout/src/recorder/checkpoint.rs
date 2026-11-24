//! Write-ahead intent for one native compaction batch. The intent contains only
//! the new JSONL bytes, their destination offset, and the source file identity.
//! Publication holds the stable thread's writer lock through directory fsync.

use std::fs;
use std::fs::File;
use std::io;
use std::io::BufWriter;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use super::RolloutLineRef;
use crate::RolloutItem;
use crate::WriterLockGuard;
use crate::ordinal::RolloutOrdinalState;

// magic, device, inode, original file length, serialized batch length
const MAGIC: &[u8; 8] = b"CODXWAL1";
const HEADER_LEN: u64 = 40;

pub(super) fn append(
    path: &Path,
    items: &[RolloutItem],
    ordinals: RolloutOrdinalState,
    lock: &WriterLockGuard,
) -> io::Result<RolloutOrdinalState> {
    lock.check_rollout_owner(path)?;
    let mut source = File::options().read(true).append(true).open(path)?;
    let next = prepare(path, &source, items, ordinals)?;
    if let Err(error) = finish(path, &mut File::open(intent_path(path))?, &mut source) {
        // A short write can leave a prefix of the batch in the source. Reopen
        // the intent from the beginning and compare that prefix before retrying.
        if !recover(path, lock)? {
            return Err(error);
        }
    }
    Ok(next)
}

pub(super) fn recover(path: &Path, lock: &WriterLockGuard) -> io::Result<bool> {
    let path = crate::plain_rollout_path(path);
    let stage = staging_path(&path);
    // Before publication only the private stage can exist. No source bytes
    // have been appended, so discard it under the stable writer lock.
    if stage.try_exists()? {
        // Compression may have finished after the writer died but before the
        // intent was published. The unpublished stage has no source bytes to repair.
        let existing = if path.try_exists()? {
            path.clone()
        } else {
            crate::compression::compressed_rollout_path(&path)
        };
        lock.check_rollout_owner(&existing)?;
        fs::remove_file(&stage)?;
        File::open(parent(&path)?)?.sync_all()?;
    }
    let intent_path = intent_path(&path);
    let metadata = match fs::symlink_metadata(&intent_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if !metadata.is_file() {
        return Err(invalid("compaction intent must be a regular file"));
    }
    lock.check_rollout_owner(&path)?;
    let mut intent = File::open(intent_path)?;
    // A previous process can die between publishing the intent and syncing the
    // directory. Make its name durable before recovery appends any bytes.
    File::open(parent(&path)?)?.sync_all()?;
    let mut source = File::options().read(true).append(true).open(&path)?;
    finish(&path, &mut intent, &mut source)?;
    Ok(true)
}

fn intent_path(path: &Path) -> PathBuf {
    crate::plain_rollout_path(path).with_added_extension("compaction-intent")
}

fn staging_path(path: &Path) -> PathBuf {
    intent_path(path).with_added_extension("stage")
}

/// Reject reads of a rollout with an unfinished compaction. An owner holding the
/// stable writer lock can finish that append and then retry the read.
pub fn ensure_checkpoint_complete(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(intent_path(path)) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "compaction append requires completion for {}",
                path.display()
            ),
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(unix)]
fn identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn identity(_: &fs::Metadata) -> (u64, u64) {
    // New intents are only written on Unix.
    (0, 0)
}

fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .ok_or_else(|| invalid("rollout has no parent directory"))
}

fn prepare(
    path: &Path,
    source: &File,
    items: &[RolloutItem],
    mut ordinals: RolloutOrdinalState,
) -> io::Result<RolloutOrdinalState> {
    let directory = parent(path)?;
    let stage = staging_path(path);
    let mut options = File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut tmp = options.open(&stage)?;
    let metadata = source.metadata()?;
    let (device, inode) = identity(&metadata);
    {
        let mut writer = BufWriter::new(&mut tmp);
        writer.write_all(MAGIC)?;
        for value in [device, inode, metadata.len(), 0] {
            writer.write_all(&value.to_le_bytes())?;
        }
        for item in items {
            serde_json::to_writer(
                &mut writer,
                &RolloutLineRef::new(item, ordinals.current()?)?,
            )?;
            writer.write_all(b"\n")?;
            ordinals.advance();
        }
        writer.flush()?;
    }
    let len = tmp.metadata()?.len() - HEADER_LEN;
    tmp.seek(SeekFrom::Start(HEADER_LEN - 8))?;
    tmp.write_all(&len.to_le_bytes())?;
    tmp.sync_all()?;
    fs::hard_link(&stage, intent_path(path))?;
    fs::remove_file(stage)?;
    File::open(directory)?.sync_all()?;
    Ok(ordinals)
}

fn replay(intent: &mut File, source: &mut File) -> io::Result<()> {
    let mut header = [0; HEADER_LEN as usize];
    intent.read_exact(&mut header)?;
    if &header[..8] != MAGIC {
        return Err(invalid("invalid compaction intent header"));
    }
    let field = |offset: usize| {
        let mut value = [0; 8];
        value.copy_from_slice(&header[offset..offset + 8]);
        u64::from_le_bytes(value)
    };
    let start = field(24);
    let len = field(32);
    let metadata = source.metadata()?;
    if identity(&metadata) != (field(8), field(16))
        || metadata.len() < start
        || metadata.len() - start > len
        || len == 0
        || intent.metadata()?.len().checked_sub(HEADER_LEN) != Some(len)
    {
        return Err(invalid(
            "compaction intent does not match its source rollout",
        ));
    }
    // A crash may leave part of any JSON record, including its final newline.
    // Compare before modifying anything and only append the remaining bytes.
    let mut existing = [0; 8192];
    let mut expected = [0; 8192];
    let mut present = metadata.len() - start;
    source.seek(SeekFrom::Start(start))?;
    while present != 0 {
        let n = present.min(existing.len() as u64) as usize;
        source.read_exact(&mut existing[..n])?;
        intent.read_exact(&mut expected[..n])?;
        if existing[..n] != expected[..n] {
            return Err(invalid(
                "rollout differs from the pending compaction intent",
            ));
        }
        present -= n as u64;
    }
    io::copy(intent, source)?;
    source.sync_all()
}

fn finish(path: &Path, intent: &mut File, source: &mut File) -> io::Result<()> {
    replay(intent, source)?;
    fs::remove_file(intent_path(path))?;
    File::open(parent(path)?)?.sync_all()
}

#[cfg(all(test, unix))]
#[path = "checkpoint_tests.rs"]
mod tests;
