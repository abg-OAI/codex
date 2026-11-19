//! Optional copy of a frozen prefix's retained tail. Native rollout files remain authoritative.
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use codex_protocol::ThreadId;
use codex_protocol::protocol::HistoryPosition;
use codex_rollout::{ModelContextScan, ModelContextScanProgress, RolloutItem};

pub(in crate::local) fn path(head: &Path) -> PathBuf {
    codex_rollout::plain_rollout_path(head).with_extension("checkpoint-tail")
}

// Bind the payload to the physical head and to the same immutable prefix its native header names.
fn checksum(
    owner: ThreadId,
    head: ThreadId,
    base: HistoryPosition,
    payload: &[u8],
) -> blake3::Hash {
    let mut hash =
        blake3::Hasher::new_derive_key("Saffrodex cold rollout checkpoint tail version 1");
    for id in [owner, head, base.thread_id] {
        hash.update(id.to_string().as_bytes());
    }
    hash.update(&base.end_ordinal_exclusive.to_le_bytes());
    hash.update(&base.end_byte_offset.to_le_bytes());
    hash.update(payload);
    hash.finalize()
}

pub(super) fn write(
    head: &Path,
    owner: ThreadId,
    id: ThreadId,
    base: HistoryPosition,
    payload: &[u8],
) -> io::Result<()> {
    let directory = head
        .parent()
        .ok_or_else(|| io::Error::other("rollout has no parent directory"))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(checksum(owner, id, base, payload).as_bytes())?;
    file.write_all(payload)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path(head))?;
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    Ok(())
}

pub(in crate::local) fn complete_scan(
    head: &Path,
    owner: ThreadId,
    base: HistoryPosition,
    mut scan: ModelContextScan,
) -> Option<Vec<RolloutItem>> {
    let id = codex_rollout::rollout_id_from_path(head)?;
    let file = File::open(path(head)).ok()?;
    let limit = super::suffix::SCAN_BYTES + 32;
    if file.metadata().ok()?.len() > limit {
        return None;
    }
    let mut buf = Vec::new();
    file.take(limit + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > limit {
        return None;
    }
    let (digest, payload) = buf.split_at_checked(32)?;
    if checksum(owner, id, base, payload).as_bytes() != digest {
        return None;
    }
    for bytes in payload.strip_suffix(b"\n")?.rsplit(|&b| b == b'\n') {
        let line = codex_rollout::parse_rollout_line_bytes(bytes).ok()?;
        if scan.push(line.item) == ModelContextScanProgress::Complete {
            return Some(scan.finish());
        }
    }
    None
}

/// The copy has no independent lifetime. Removing it leaves the native ancestry readable.
pub(in crate::local) fn remove(head: &Path) -> io::Result<()> {
    match std::fs::remove_file(path(head)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}
