//! Consolidates immutable ranges from one thread into an ordinary native rollout.
//!
//! The first header still references the prefix preceding the archived ranges.
//! Subsequent records keep their original bytes and ordinals, including segment
//! headers. Existing continuations and forks keep pointing at the original files.

use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::HistoryPosition;

use super::LocalThreadStore;
use super::rollout_lineage::RolloutLineageSegment;
use super::thread_history;
use super::thread_history_materialization;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

/// A sealed, indexed file that can serve as the history base for a new writer.
/// Its owning continuation retains it through any uncertain publication.
#[derive(Clone)]
pub(super) struct RolloutArchive {
    pub(super) position: HistoryPosition,
    path: PathBuf,
}

impl RolloutArchive {
    /// Returns no archive when consolidating this thread cannot shorten its visible chain.
    /// An attached thread's hidden parent and another thread's segments stay behind their
    /// original bookmarks. Calls hold the selected writer's lock through publication.
    pub(super) async fn prepare(
        store: &LocalThreadStore,
        owner: ThreadId,
        selected_path: &Path,
        selected: HistoryPosition,
        rotations_remaining: Option<usize>,
        limit: usize,
    ) -> ThreadStoreResult<(usize, Option<Self>)> {
        if let Some(remaining) = rotations_remaining.and_then(|n| n.checked_sub(1)) {
            return Ok((remaining, None));
        }
        let lineage = store
            .resolve_rollout_lineage_for_history_page(owner)
            .await?;
        let segments = lineage.segments();
        if rotations_remaining.is_none() && segments.len() < limit {
            return Ok((limit - segments.len() - 1, None));
        }
        if !segments.last().is_some_and(|segment| {
            segment.rollout_id == selected.thread_id && segment.rollout_path == selected_path
        }) {
            return Err(archive_error(
                "selected rollout changed during archive preparation",
            ));
        }
        let Some((first, bytes)) = archive_suffix(segments, owner, selected, limit) else {
            // A foreign prefix may itself exceed the stock paging limit. Accumulate
            // another batch of local segments before walking that prefix again.
            return Ok((limit / 4, None));
        };
        let id = ThreadId::new();
        let path = codex_rollout::sibling_rollout_path(selected_path, id).ok_or_else(|| {
            archive_error("cannot name a native archive beside the selected rollout")
        })?;
        let archive = Self {
            position: HistoryPosition {
                thread_id: id,
                end_byte_offset: bytes,
                end_ordinal_exclusive: selected.end_ordinal_exclusive,
            },
            path,
        };
        let ranges = segments[first..]
            .iter()
            .map(|segment| {
                (
                    segment.rollout_path.clone(),
                    segment.end.unwrap_or(selected).end_byte_offset,
                )
            })
            .collect();
        let path = archive.path.clone();
        tokio::task::spawn_blocking(move || copy_ranges(ranges, &path))
            .await
            .map_err(archive_error)?
            .map_err(archive_error)?;
        // Vanilla pages this physical ID directly in SQLite. Publish only after
        // its native projection covers the same bytes and ordinals as the bookmark.
        let project = async {
            thread_history_materialization::materialize_to_sqlite(store, id, &archive.path).await?;
            let state = thread_history::projection_state(store, id).await?;
            if !state.is_some_and(|state| {
                state.next_ordinal == selected.end_ordinal_exclusive
                    && state.next_byte_offset == bytes
            }) {
                return Err(archive_error("incomplete archive history projection"));
            }
            Ok(())
        }
        .await;
        if let Err(error) = project {
            archive.discard(store).await?;
            return Err(error);
        }
        Ok((limit.saturating_sub(first + 2), Some(archive)))
    }

    /// Removes only a new archive whose continuation is known to be unpublished.
    pub(super) async fn discard(&self, store: &LocalThreadStore) -> ThreadStoreResult<()> {
        thread_history::delete_thread(store, self.position.thread_id).await?;
        match tokio::fs::remove_file(&self.path).await {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result.map_err(archive_error),
        }
    }
}

/// Keep the original root and any foreign or inherited prefix. Consolidate a
/// quarter of the segment budget when available, then stop before recopying
/// an older range that is larger than the suffix already selected.
fn archive_suffix(
    segments: &[RolloutLineageSegment],
    owner: ThreadId,
    selected: HistoryPosition,
    limit: usize,
) -> Option<(usize, u64)> {
    let mut first = segments.len();
    let mut bytes = 0u64;
    for (index, segment) in segments.iter().enumerate().rev() {
        if index == 0
            || segment.rollout_id == owner
            || codex_rollout::thread_id_from_rollout_path(&segment.rollout_path) != Some(owner)
        {
            break;
        }
        let size = segment.end.unwrap_or(selected).end_byte_offset;
        if segments.len() - first >= (limit / 4).max(2) && size > bytes {
            break;
        }
        first = index;
        bytes = bytes.checked_add(size)?;
    }
    let packed = segments.len() - first;
    (packed >= 2 && (first + 2 <= limit || packed >= limit / 4)).then_some((first, bytes))
}

/// Copy only the newline-terminated bytes admitted by each immutable bookmark.
/// Renaming the temporary file makes a complete archive visible to native readers.
fn copy_ranges(ranges: Vec<(PathBuf, u64)>, destination: &Path) -> io::Result<()> {
    let directory = destination
        .parent()
        .ok_or_else(|| io::Error::other("archive has no parent directory"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    for (path, bytes) in ranges {
        let mut source = codex_rollout::open_rollout_stream(&path)?;
        let prefix_len = bytes
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("empty archive range"))?;
        if io::copy(
            &mut source.by_ref().take(prefix_len),
            temporary.as_file_mut(),
        )? != prefix_len
        {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete archive range",
            ));
        }
        let mut terminator = [0];
        source.read_exact(&mut terminator)?;
        if terminator != [b'\n'] {
            return Err(io::Error::other(
                "archive range ends inside a rollout record",
            ));
        }
        temporary.write_all(&terminator)?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(destination)?;
    #[cfg(unix)]
    if let Err(error) = std::fs::File::open(directory).and_then(|dir| dir.sync_all()) {
        std::fs::remove_file(destination)?;
        return Err(error);
    }
    Ok(())
}

/// Realtime cursors bind the physical head on which their snapshot was observed.
/// A sealed range may now contain that head's original records. Its unchanged
/// header identifies the inherited prefix. Compare the following records through
/// the saved snapshot so a divergent revert cannot reuse the cursor.
pub(super) async fn contains_snapshot(
    home: &Path,
    owner: ThreadId,
    head: ThreadId,
    snapshot_end: u64,
    mut sealed_ranges: impl Iterator<Item = (ThreadId, u64, u64)>,
) -> ThreadStoreResult<bool> {
    let Some(original) = codex_rollout::find_rollout_path_by_rollout_id(home, head)
        .await
        .map_err(archive_error)?
    else {
        return Ok(false);
    };
    if codex_rollout::thread_id_from_rollout_path(&original) != Some(owner) {
        return Ok(false);
    }
    let header = codex_rollout::read_session_meta_line(&original)
        .await
        .map_err(archive_error)?;
    if header.meta.id != owner {
        return Ok(false);
    }
    let header_ordinal = header
        .meta
        .history_base
        .map_or(0, |base| base.end_ordinal_exclusive);
    let Some((archive, _, _)) = sealed_ranges.find(|(_, start, end)| {
        *start <= header_ordinal.saturating_add(1) && header_ordinal < *end && snapshot_end <= *end
    }) else {
        return Ok(false);
    };
    let Some(path) = codex_rollout::find_rollout_path_by_rollout_id(home, archive)
        .await
        .map_err(archive_error)?
    else {
        return Ok(false);
    };
    tokio::task::spawn_blocking(move || prefix_matches(&original, &path, snapshot_end))
        .await
        .map_err(archive_error)?
        .map_err(archive_error)
}

fn prefix_matches(original: &Path, archive: &Path, snapshot_end: u64) -> io::Result<bool> {
    let mut original = BufReader::new(codex_rollout::open_rollout_stream(original)?);
    let mut archive = BufReader::new(codex_rollout::open_rollout_stream(archive)?);
    let mut source_line = Vec::new();
    let mut archived_line = Vec::new();
    if original.read_until(b'\n', &mut source_line)? == 0 {
        return Ok(false);
    }
    loop {
        archived_line.clear();
        if archive.read_until(b'\n', &mut archived_line)? == 0 {
            return Ok(false);
        }
        if archived_line == source_line {
            break;
        }
    }
    // The snapshot can end before this head's first item when the caller retains
    // an earlier snapshot bound. Even then its physical header must be visible.
    loop {
        if archived_line != source_line {
            return Ok(false);
        }
        if codex_rollout::parse_rollout_line_bytes(&source_line)
            .ok()
            .and_then(|line| line.ordinal)
            .is_some_and(|ordinal| ordinal.saturating_add(1) >= snapshot_end)
        {
            return Ok(true);
        }
        source_line.clear();
        archived_line.clear();
        if original.read_until(b'\n', &mut source_line)? == 0
            || archive.read_until(b'\n', &mut archived_line)? == 0
        {
            return Ok(false);
        }
    }
}

fn archive_error(error: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("cannot consolidate rollout history: {error}"),
    }
}
