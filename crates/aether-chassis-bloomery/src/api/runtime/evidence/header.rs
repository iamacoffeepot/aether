//! One dispatch's evidence header: capped prose, process identity, file list.

use std::fs;
use std::path::Path;

use serde::Deserialize;

use super::{SWEPT_NOTICE, evidence_dir, evidence_retained, nonce_spellings};
use crate::api::dto::{DispatchEvidenceView, DispatchProcessView};

/// Independent cap on `assistant_text` in the header.
pub const ASSISTANT_TEXT_CAP: usize = 16 * 1024;
/// Independent cap on `commit_message` in the header.
pub const COMMIT_MESSAGE_CAP: usize = 4 * 1024;
const FILE_LIST_CAP: usize = 64;
const IDENTITY_FILE: &str = "identity";
const EVIDENCE_JSON: &str = "evidence.json";

/// Read the header for a nonce the journal already named.
pub fn read(worktree_base: &Path, nonce: &str) -> DispatchEvidenceView {
    let spelling = nonce_spellings(nonce)
        .into_iter()
        .find(|candidate| evidence_retained(worktree_base, candidate))
        .unwrap_or_else(|| nonce.to_owned());
    if !evidence_retained(worktree_base, &spelling) {
        return DispatchEvidenceView {
            nonce: spelling,
            retained: false,
            notice: Some(SWEPT_NOTICE.to_owned()),
            assistant_text: None,
            assistant_text_truncated: false,
            commit_message: None,
            commit_message_truncated: false,
            process: None,
            files: Vec::new(),
        };
    }

    let dir = evidence_dir(worktree_base, &spelling);
    let files = list_files(&dir);
    let process = read_process(&dir.join(IDENTITY_FILE));
    let (assistant_text, assistant_text_truncated, commit_message, commit_message_truncated) =
        read_prose(&dir.join(EVIDENCE_JSON));
    DispatchEvidenceView {
        nonce: spelling,
        retained: true,
        notice: None,
        assistant_text,
        assistant_text_truncated,
        commit_message,
        commit_message_truncated,
        process,
        files,
    }
}

fn list_files(dir: &Path) -> Vec<String> {
    let mut names = match fs::read_dir(dir) {
        Ok(entries) => {
            entries.flatten().filter_map(|entry| entry.file_name().to_str().map(str::to_owned)).collect::<Vec<_>>()
        }
        Err(_) => return Vec::new(),
    };
    names.sort();
    names.truncate(FILE_LIST_CAP);
    names
}

fn read_process(path: &Path) -> Option<DispatchProcessView> {
    let body = fs::read_to_string(path).ok()?;
    serde_json::from_str(body.trim()).ok()
}

#[derive(Deserialize)]
struct EvidenceProse {
    #[serde(default)]
    assistant_text: Option<String>,
    #[serde(default)]
    commit_message: Option<String>,
    #[serde(default)]
    result_record: Option<ResultRecordProse>,
}

#[derive(Deserialize)]
struct ResultRecordProse {
    #[serde(default)]
    assistant_text: Option<String>,
}

fn read_prose(path: &Path) -> (Option<String>, bool, Option<String>, bool) {
    let Ok(bytes) = fs::read(path) else {
        return (None, false, None, false);
    };
    let Ok(value) = serde_json::from_slice::<EvidenceProse>(&bytes) else {
        return (None, false, None, false);
    };
    let assistant = value
        .assistant_text
        .filter(|text| !text.is_empty())
        .or_else(|| value.result_record.and_then(|record| record.assistant_text.filter(|text| !text.is_empty())));
    let commit = value.commit_message.map(|text| text.trim().to_owned()).filter(|text| !text.is_empty());
    let (assistant_text, assistant_text_truncated) = cap_text(assistant.as_deref(), ASSISTANT_TEXT_CAP);
    let (commit_message, commit_message_truncated) = cap_text(commit.as_deref(), COMMIT_MESSAGE_CAP);
    (assistant_text, assistant_text_truncated, commit_message, commit_message_truncated)
}

fn cap_text(text: Option<&str>, cap: usize) -> (Option<String>, bool) {
    let Some(text) = text else {
        return (None, false);
    };
    if text.len() <= cap {
        return (Some(text.to_owned()), false);
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (Some(text[..end].to_owned()), true)
}
