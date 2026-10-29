//! The OSC 7501 program status protocol, which programs such as Claude Code use to report
//! whether they are working, waiting on the user, or done:
//! `ESC ] 7501 ; state=<state>[:<key>=<value>]... ST`.
//!
//! Programs only start reporting after the terminal answers the `ESC ] 7501 ; ? ST` query.

/// Reply to the support query. Programs accept any OSC 7501 reply whose data starts with `?`.
pub(crate) const PROGRAM_STATUS_QUERY_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

const PREFIX: &[u8] = b"\x1b]7501;";
const ESCAPE: u8 = 0x1b;
const BELL: u8 = 0x07;
/// Larger payloads (a `msg` is at most 2 KiB of base64) are dropped rather than buffered.
const MAX_PAYLOAD_LEN: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramState {
    Working,
    /// Waiting on the user, e.g. for a permission prompt or an answer.
    Blocked,
    Done,
    Idle,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProgramStatusSequence {
    Query,
    /// `None` when the program cleared its status, e.g. on exit.
    State(Option<ProgramState>),
}

/// Finds program status sequences in a stream of PTY output, which may split them across reads.
#[derive(Default)]
pub(crate) struct ProgramStatusScanner {
    prefix_matched: usize,
    payload: Option<Vec<u8>>,
    pending_escape: bool,
}

impl ProgramStatusScanner {
    pub(crate) fn scan(
        &mut self,
        bytes: &[u8],
        mut on_sequence: impl FnMut(ProgramStatusSequence),
    ) {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            if self.payload.is_none() && self.prefix_matched == 0 {
                // Fast path for ordinary output: nothing can start before the next escape.
                let Some(escape_index) = remaining.iter().position(|byte| *byte == ESCAPE) else {
                    return;
                };
                remaining = &remaining[escape_index..];
            }
            let Some((&byte, rest)) = remaining.split_first() else {
                return;
            };
            remaining = rest;
            if let Some(sequence) = self.push(byte) {
                on_sequence(sequence);
            }
        }
    }

    fn push(&mut self, byte: u8) -> Option<ProgramStatusSequence> {
        let Some(payload) = self.payload.as_mut() else {
            self.match_prefix(byte);
            return None;
        };

        if self.pending_escape {
            self.pending_escape = false;
            if byte == b'\\' {
                return self
                    .payload
                    .take()
                    .and_then(|payload| parse_payload(&payload));
            }
            // An escape that does not terminate the sequence starts a new one.
            self.payload = None;
            self.prefix_matched = 1;
            self.match_prefix(byte);
            return None;
        }

        match byte {
            BELL => self
                .payload
                .take()
                .and_then(|payload| parse_payload(&payload)),
            ESCAPE => {
                self.pending_escape = true;
                None
            }
            _ if payload.len() >= MAX_PAYLOAD_LEN => {
                self.payload = None;
                None
            }
            _ => {
                payload.push(byte);
                None
            }
        }
    }

    fn match_prefix(&mut self, byte: u8) {
        if PREFIX.get(self.prefix_matched) == Some(&byte) {
            self.prefix_matched += 1;
            if self.prefix_matched == PREFIX.len() {
                self.prefix_matched = 0;
                self.payload = Some(Vec::new());
            }
        } else {
            self.prefix_matched = usize::from(byte == ESCAPE);
        }
    }
}

fn parse_payload(payload: &[u8]) -> Option<ProgramStatusSequence> {
    let payload = std::str::from_utf8(payload).ok()?;
    if payload.starts_with('?') {
        return Some(ProgramStatusSequence::Query);
    }

    let mut state = None;
    for field in payload.split(':') {
        match field.split_once('=') {
            // Sequences with an id report on a program's background tasks rather than the
            // program itself.
            Some(("id", _)) => return None,
            Some(("state", value)) => state = Some(value),
            _ => {}
        }
    }

    let state = match state? {
        "working" => Some(ProgramState::Working),
        "blocked" => Some(ProgramState::Blocked),
        "done" => Some(ProgramState::Done),
        "idle" => Some(ProgramState::Idle),
        "error" => Some(ProgramState::Error),
        "clear" => None,
        _ => return None,
    };
    Some(ProgramStatusSequence::State(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_chunks(chunks: &[&[u8]]) -> Vec<ProgramStatusSequence> {
        let mut scanner = ProgramStatusScanner::default();
        let mut sequences = Vec::new();
        for chunk in chunks {
            scanner.scan(chunk, |sequence| sequences.push(sequence));
        }
        sequences
    }

    #[test]
    fn test_parses_states_with_either_terminator() {
        assert_eq!(
            scan_chunks(&[
                b"output\x1b]7501;state=working:app=claude-code:progress=40\x1b\\more",
                b"\x1b]7501;state=blocked:app=claude-code:kind=permission:msg=QWxsb3c=\x07",
                b"\x1b]7501;state=done:app=claude-code\x1b\\",
                b"\x1b]7501;state=idle\x1b\\\x1b]7501;state=error\x1b\\",
                b"\x1b]7501;state=clear\x1b\\",
            ]),
            [
                ProgramStatusSequence::State(Some(ProgramState::Working)),
                ProgramStatusSequence::State(Some(ProgramState::Blocked)),
                ProgramStatusSequence::State(Some(ProgramState::Done)),
                ProgramStatusSequence::State(Some(ProgramState::Idle)),
                ProgramStatusSequence::State(Some(ProgramState::Error)),
                ProgramStatusSequence::State(None),
            ]
        );
    }

    #[test]
    fn test_detects_query() {
        assert_eq!(
            scan_chunks(&[b"\x1b[>0q\x1b]7501;?\x1b\\\x1b[c"]),
            [ProgramStatusSequence::Query]
        );
    }

    #[test]
    fn test_handles_sequences_split_across_reads() {
        assert_eq!(
            scan_chunks(&[b"abc\x1b", b"]75", b"01;sta", b"te=done", b"\x1b", b"\\"]),
            [ProgramStatusSequence::State(Some(ProgramState::Done))]
        );
    }

    #[test]
    fn test_ignores_other_sequences_and_background_tasks() {
        assert_eq!(
            scan_chunks(&[
                b"\x1b]0;title\x07\x1b]750;state=done\x07\x1b]75011;state=done\x07",
                b"\x1b]7501;state=working:app=claude-code:id=task-1\x1b\\",
                b"\x1b]7501;state=sleeping\x1b\\\x1b]7501;app=claude-code\x1b\\",
            ]),
            []
        );
    }

    #[test]
    fn test_recovers_from_malformed_sequences() {
        let oversized = [b'a'; MAX_PAYLOAD_LEN + 1];
        assert_eq!(
            scan_chunks(&[
                b"\x1b]7501;state=working\x1b[0m",
                b"\x1b]7501;",
                &oversized,
                b"\x07\x1b]7501;state=idle\x07",
            ]),
            [ProgramStatusSequence::State(Some(ProgramState::Idle))],
            "an unterminated or oversized sequence is dropped without losing the next one"
        );
    }
}
