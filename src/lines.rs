//! Reading newline-delimited messages without trusting their length.
//!
//! `read_line` grows its buffer for as long as the peer keeps sending, so a
//! line without an end would take all memory. Both the socket and the
//! agent's stdout go through this instead.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// A complete line, without its newline (a final line without one counts).
    Text(String),
    /// A line longer than the limit; it has been read and thrown away.
    TooLong(usize),
    /// Bytes that are not UTF-8.
    NotUtf8,
    Eof,
}

/// Read one line of at most `limit` bytes.
pub async fn read<R: AsyncBufRead + Unpin>(reader: &mut R, limit: usize) -> std::io::Result<Line> {
    let mut line = Vec::new();
    let mut dropped = 0usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if dropped > 0 {
                return Ok(Line::TooLong(dropped));
            }
            if line.is_empty() {
                return Ok(Line::Eof);
            }
            break;
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let (chunk, done) = match newline {
            Some(i) => (&available[..i], i + 1),
            None => (available, available.len()),
        };
        let complete = newline.is_some();
        if dropped > 0 || line.len() + chunk.len() > limit {
            dropped += line.len() + chunk.len();
            line.clear();
        } else {
            line.extend_from_slice(chunk);
        }
        reader.consume(done);
        if complete {
            if dropped > 0 {
                return Ok(Line::TooLong(dropped));
            }
            break;
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(String::from_utf8(line).map_or(Line::NotUtf8, Line::Text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    async fn all(input: &[u8], limit: usize, capacity: usize) -> Vec<Line> {
        let mut reader = BufReader::with_capacity(capacity, input);
        let mut out = Vec::new();
        loop {
            let line = read(&mut reader, limit).await.unwrap();
            let eof = line == Line::Eof;
            out.push(line);
            if eof {
                return out;
            }
        }
    }

    #[tokio::test]
    async fn splits_lines_and_keeps_the_last_one() {
        for capacity in [1, 3, 64] {
            assert_eq!(
                all(b"one\ntwo\r\nthree", 100, capacity).await,
                vec![
                    Line::Text("one".into()),
                    Line::Text("two".into()),
                    Line::Text("three".into()),
                    Line::Eof
                ],
                "capacity {capacity}"
            );
        }
    }

    #[tokio::test]
    async fn an_oversized_line_is_skipped_and_reading_goes_on() {
        for capacity in [1, 4, 64] {
            let lines = all(b"short\n0123456789abc\nnext\n", 8, capacity).await;
            assert_eq!(
                lines,
                vec![
                    Line::Text("short".into()),
                    Line::TooLong(13),
                    Line::Text("next".into()),
                    Line::Eof
                ],
                "capacity {capacity}"
            );
        }
    }

    #[tokio::test]
    async fn invalid_utf8_is_reported_not_fatal() {
        let lines = all(b"\xff\xfe\nok\n", 100, 64).await;
        assert_eq!(
            lines,
            vec![Line::NotUtf8, Line::Text("ok".into()), Line::Eof]
        );
    }

    #[tokio::test]
    async fn empty_lines_are_lines() {
        assert_eq!(
            all(b"\n\nx\n", 100, 2).await,
            vec![
                Line::Text(String::new()),
                Line::Text(String::new()),
                Line::Text("x".into()),
                Line::Eof
            ]
        );
    }
}
