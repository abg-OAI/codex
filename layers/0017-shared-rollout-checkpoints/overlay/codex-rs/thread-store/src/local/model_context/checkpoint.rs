//! Probes native checkpoint tails before expanding immutable rollout ancestry.

use super::*;

/// Probe a small active segment before resolving any inherited history.
///
/// The native compaction boundary has the same semantics in the ordinary lineage scan.
/// Compressed files and suffixes beyond the probe budget retain that scan.
/// This limit bounds speculative work; it never truncates the context returned to callers.
pub(in crate::local) async fn scan_current_checkpoint(
    path: &Path,
) -> ThreadStoreResult<Option<Vec<RolloutItem>>> {
    scan_checkpoint(path, None).await
}

/// Reads a frozen native prefix without first expanding its ancestry.
pub(in crate::local) async fn scan_checkpoint(
    path: &Path,
    end_byte_offset: Option<u64>,
) -> ThreadStoreResult<Option<Vec<RolloutItem>>> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            return Ok(None);
        }
        let file = CheckpointProbe {
            file,
            remaining: 64 * 1024 * 1024,
        };
        let mut scanner = match end_byte_offset {
            Some(end) => ReverseJsonlScanner::new_at(file, end)?,
            None => ReverseJsonlScanner::new(file)?,
        };
        let mut scan = ModelContextScan::default();
        loop {
            let outcome = match scanner.scan_next_rollout_line() {
                Ok(Some(outcome)) => outcome,
                Ok(None) => break,
                Err(error)
                    if error
                        .get_ref()
                        .is_some_and(|error| error.is::<CheckpointProbeLimit>()) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            let ScanOutcome::Parsed(line) = outcome else {
                continue;
            };
            if matches!(line.item, RolloutItem::SessionMeta(_)) {
                break;
            }
            if scan.push(line.item) == ModelContextScanProgress::Complete {
                return Ok(Some(scan.finish()));
            }
        }
        Ok::<_, io::Error>(None)
    })
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to join current checkpoint scan: {err}"),
    })?
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to scan current checkpoint: {err}"),
    })
}

/// Caps speculative reads; exceeding the cap selects the established lineage reader.
struct CheckpointProbe {
    file: std::fs::File,
    remaining: usize,
}

impl io::Read for CheckpointProbe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::other(CheckpointProbeLimit));
        }
        let limit = bytes.len().min(self.remaining);
        let read = io::Read::read(&mut self.file, &mut bytes[..limit])?;
        self.remaining -= read;
        Ok(read)
    }
}

impl io::Seek for CheckpointProbe {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        io::Seek::seek(&mut self.file, position)
    }
}

/// Distinguishes the optimization's limit from real storage errors.
#[derive(Debug)]
struct CheckpointProbeLimit;

impl std::fmt::Display for CheckpointProbeLimit {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("checkpoint probe exceeded its read budget")
    }
}

impl std::error::Error for CheckpointProbeLimit {}
