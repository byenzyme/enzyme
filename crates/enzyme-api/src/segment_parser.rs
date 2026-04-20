//! Greedy streaming JSON segment extractor.
//!
//! Consumes a stream of string tokens and yields complete JSON objects
//! from within a top-level JSON array. This is a port of the Python
//! `_extract_segments_greedily()` function.

use serde::Deserialize;

/// A single synthesis segment parsed from the LLM's streaming JSON output.
#[derive(Debug, Clone, Deserialize)]
pub struct SynthesisSegment {
    pub explanation: Option<String>,
    pub cited_documents: Option<Vec<CitedDocument>>,
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct CitedDocument {
    pub doc_id: Option<String>,
    pub file_path: Option<String>,
    pub excerpt: Option<String>,
    pub heading: Option<String>,
    pub block_ref: Option<String>,
}

/// State machine for greedy segment extraction.
enum ParseState {
    /// Looking for `[` to start the segments array.
    Scanning,
    /// Inside the array, looking for `{` to start an object.
    InArray,
    /// Inside an object, tracking brace depth.
    InObject {
        depth: usize,
        in_string: bool,
        escape_next: bool,
    },
    /// Array closed, done.
    Done,
}

/// Extract complete JSON segment objects from a stream of tokens.
///
/// Yields each complete JSON object as a `SynthesisSegment` as soon as the
/// closing `}` is found (greedy extraction).
#[allow(dead_code)]
pub fn extract_segments(tokens: &[String]) -> Vec<SynthesisSegment> {
    let mut state = ParseState::Scanning;
    let mut object_buf = String::new();
    let mut segments = Vec::new();

    for token in tokens {
        for ch in token.chars() {
            match &mut state {
                ParseState::Scanning => {
                    if ch == '[' {
                        state = ParseState::InArray;
                    }
                }
                ParseState::InArray => {
                    if ch == '{' {
                        object_buf.clear();
                        object_buf.push(ch);
                        state = ParseState::InObject {
                            depth: 1,
                            in_string: false,
                            escape_next: false,
                        };
                    } else if ch == ']' {
                        state = ParseState::Done;
                    }
                    // Skip whitespace and commas between objects
                }
                ParseState::InObject {
                    depth,
                    in_string,
                    escape_next,
                } => {
                    object_buf.push(ch);

                    if *escape_next {
                        *escape_next = false;
                        continue;
                    }

                    if *in_string {
                        match ch {
                            '\\' => *escape_next = true,
                            '"' => *in_string = false,
                            _ => {}
                        }
                    } else {
                        match ch {
                            '"' => *in_string = true,
                            '{' => *depth += 1,
                            '}' => {
                                *depth -= 1;
                                if *depth == 0 {
                                    // Complete object — try to parse it
                                    if let Ok(segment) =
                                        serde_json::from_str::<SynthesisSegment>(&object_buf)
                                    {
                                        segments.push(segment);
                                    }
                                    object_buf.clear();
                                    state = ParseState::InArray;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                ParseState::Done => {}
            }
        }
    }

    segments
}

/// Incremental segment extractor that can be fed tokens one at a time.
pub struct SegmentExtractor {
    state: ParseState,
    object_buf: String,
}

impl SegmentExtractor {
    pub fn new() -> Self {
        Self {
            state: ParseState::Scanning,
            object_buf: String::new(),
        }
    }

    /// Feed a token and return any complete segments found.
    pub fn feed(&mut self, token: &str) -> Vec<SynthesisSegment> {
        let mut segments = Vec::new();

        for ch in token.chars() {
            match &mut self.state {
                ParseState::Scanning => {
                    if ch == '[' {
                        self.state = ParseState::InArray;
                    }
                }
                ParseState::InArray => {
                    if ch == '{' {
                        self.object_buf.clear();
                        self.object_buf.push(ch);
                        self.state = ParseState::InObject {
                            depth: 1,
                            in_string: false,
                            escape_next: false,
                        };
                    } else if ch == ']' {
                        self.state = ParseState::Done;
                    }
                }
                ParseState::InObject {
                    depth,
                    in_string,
                    escape_next,
                } => {
                    self.object_buf.push(ch);

                    if *escape_next {
                        *escape_next = false;
                        continue;
                    }

                    if *in_string {
                        match ch {
                            '\\' => *escape_next = true,
                            '"' => *in_string = false,
                            _ => {}
                        }
                    } else {
                        match ch {
                            '"' => *in_string = true,
                            '{' => *depth += 1,
                            '}' => {
                                *depth -= 1;
                                if *depth == 0 {
                                    if let Ok(segment) =
                                        serde_json::from_str::<SynthesisSegment>(&self.object_buf)
                                    {
                                        segments.push(segment);
                                    }
                                    self.object_buf.clear();
                                    self.state = ParseState::InArray;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                ParseState::Done => {}
            }
        }

        segments
    }
}
