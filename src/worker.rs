//! Background note processing: parse the note and emit its LaTeX off the typing path.
//!
//! Requests are coalesced, so a burst of edits costs one build of the newest text.

use std::sync::mpsc;

use crate::app::LoopEvent;
use crate::document::Document;
use crate::latex::{LatexDocument, emit_latex};
use crate::note::NoteError;

pub(crate) struct BuildRequest {
    pub revision: u64,
    pub source: String,
}

pub(crate) enum WorkerEvent {
    Built {
        revision: u64,
        document: LatexDocument,
        words: usize,
    },
    ParseFailed {
        revision: u64,
        message: String,
        byte: usize,
    },
}

pub(crate) fn build_worker(
    requests: mpsc::Receiver<BuildRequest>,
    events: mpsc::Sender<LoopEvent>,
) {
    while let Ok(mut request) = requests.recv() {
        while let Ok(newer) = requests.try_recv() {
            request = newer;
        }
        let event = match Document::parse(&request.source) {
            Ok(document) => WorkerEvent::Built {
                revision: request.revision,
                document: emit_latex(&document),
                words: request.source.split_whitespace().count(),
            },
            Err(error) => {
                let byte = match &error {
                    NoteError::InvalidMath { byte, .. } => *byte,
                };
                WorkerEvent::ParseFailed {
                    revision: request.revision,
                    message: error.to_string(),
                    byte,
                }
            }
        };
        if events.send(LoopEvent::Worker(event)).is_err() {
            return;
        }
    }
}
