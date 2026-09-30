// Adapted from dotenvy 0.15.7 (MIT); see LICENSE in this directory.
// Keep syntax and line handling aligned with upstream; only environment lookup is injected.
use dotenvy::Error;
use std::io::BufRead;
type Result<T> = std::result::Result<T, Error>;
pub(super) struct QuotedLines<B> {
    buf: B,
}

enum ParseState {
    Complete,
    Escape,
    StrongOpen,
    StrongOpenEscape,
    WeakOpen,
    WeakOpenEscape,
    Comment,
    WhiteSpace,
}

fn eval_end_state(prev_state: ParseState, buf: &str) -> (usize, ParseState) {
    let mut cur_state = prev_state;
    let mut cur_pos: usize = 0;

    for (pos, c) in buf.char_indices() {
        cur_pos = pos;
        cur_state = match cur_state {
            ParseState::WhiteSpace => match c {
                '#' => return (cur_pos, ParseState::Comment),
                '\\' => ParseState::Escape,
                '"' => ParseState::WeakOpen,
                '\'' => ParseState::StrongOpen,
                _ => ParseState::Complete,
            },
            ParseState::Escape => ParseState::Complete,
            ParseState::Complete => match c {
                c if c.is_whitespace() && c != '\n' && c != '\r' => ParseState::WhiteSpace,
                '\\' => ParseState::Escape,
                '"' => ParseState::WeakOpen,
                '\'' => ParseState::StrongOpen,
                _ => ParseState::Complete,
            },
            ParseState::WeakOpen => match c {
                '\\' => ParseState::WeakOpenEscape,
                '"' => ParseState::Complete,
                _ => ParseState::WeakOpen,
            },
            ParseState::WeakOpenEscape => ParseState::WeakOpen,
            ParseState::StrongOpen => match c {
                '\\' => ParseState::StrongOpenEscape,
                '\'' => ParseState::Complete,
                _ => ParseState::StrongOpen,
            },
            ParseState::StrongOpenEscape => ParseState::StrongOpen,
            // Comments last the entire line.
            ParseState::Comment => panic!("should have returned early"),
        };
    }
    (cur_pos, cur_state)
}

impl<B: BufRead> Iterator for QuotedLines<B> {
    type Item = Result<String>;

    fn next(&mut self) -> Option<Result<String>> {
        let mut buf = String::new();
        let mut cur_state = ParseState::Complete;
        let mut buf_pos;
        let mut cur_pos;
        loop {
            buf_pos = buf.len();
            match self.buf.read_line(&mut buf) {
                Ok(0) => match cur_state {
                    ParseState::Complete => return None,
                    _ => {
                        let len = buf.len();
                        return Some(Err(Error::LineParse(buf, len)));
                    }
                },
                Ok(_n) => {
                    // Skip lines which start with a # before iteration
                    // This optimizes parsing a bit.
                    if buf.trim_start().starts_with('#') {
                        return Some(Ok(String::with_capacity(0)));
                    }
                    let result = eval_end_state(cur_state, &buf[buf_pos..]);
                    cur_pos = result.0;
                    cur_state = result.1;

                    match cur_state {
                        ParseState::Complete => {
                            if buf.ends_with('\n') {
                                buf.pop();
                                if buf.ends_with('\r') {
                                    buf.pop();
                                }
                            }
                            return Some(Ok(buf));
                        }
                        ParseState::Escape
                        | ParseState::StrongOpen
                        | ParseState::StrongOpenEscape
                        | ParseState::WeakOpen
                        | ParseState::WeakOpenEscape
                        | ParseState::WhiteSpace => {}
                        ParseState::Comment => {
                            buf.truncate(buf_pos + cur_pos);
                            return Some(Ok(buf));
                        }
                    }
                }
                Err(e) => return Some(Err(Error::Io(e))),
            }
        }
    }
}

impl<R: std::io::Read> QuotedLines<std::io::BufReader<R>> {
    pub(super) fn new(reader: R) -> Self {
        Self {
            buf: std::io::BufReader::new(reader),
        }
    }
}
