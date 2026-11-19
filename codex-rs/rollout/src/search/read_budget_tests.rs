use super::*;
use std::fs::File;
use std::io::Seek;
use std::io::Write;

fn file(bytes: &[u8]) -> io::Result<File> {
    let mut file = tempfile::tempfile()?;
    file.write_all(bytes)?;
    file.rewind()?;
    Ok(file)
}

#[test]
fn plain_files_share_one_allowance_and_can_end_at_the_boundary() -> io::Result<()> {
    let budget = Arc::new(SearchReadBudget::new(7));
    for bytes in [b"abc".as_slice(), b"defg", b""] {
        let count = io::copy(&mut budget.reader(file(bytes)?, false)?, &mut io::sink())?;
        assert_eq!(count, bytes.len() as u64);
    }

    let err = budget
        .reader(file(b"h")?, false)?
        .read_to_end(&mut Vec::new())
        .unwrap_err();
    assert!(err.to_string().contains("64 MiB"));
    assert!(budget.exceeded.load(Ordering::Relaxed));
    Ok(())
}

#[test]
fn plain_and_compressed_files_share_the_decoded_limit() -> io::Result<()> {
    let budget = Arc::new(SearchReadBudget::new(4096));
    io::copy(
        &mut budget.reader(file(&[0; 2048])?, false)?,
        &mut io::sink(),
    )?;
    let compressed = zstd::stream::encode_all(&[0_u8; 2049][..], 1)?;
    let mut output = Vec::new();
    assert!(
        budget
            .reader(file(&compressed)?, true)?
            .read_to_end(&mut output)
            .is_err()
    );
    assert_eq!(output.len(), 2048);
    assert!(budget.exceeded.load(Ordering::Relaxed));
    Ok(())
}

#[test]
fn compressed_input_is_limited_even_when_it_produces_no_records() -> io::Result<()> {
    let frame = zstd::stream::encode_all(io::empty(), 1)?;
    let budget = Arc::new(SearchReadBudget::new(frame.len() as u64));
    let mut bytes = frame.clone();
    bytes.extend_from_slice(&frame);
    let result = (|| -> io::Result<()> {
        budget
            .reader(file(&bytes)?, true)?
            .read_to_end(&mut Vec::new())?;
        Ok(())
    })();
    assert!(result.is_err());
    assert!(budget.exceeded.load(Ordering::Relaxed));
    Ok(())
}
