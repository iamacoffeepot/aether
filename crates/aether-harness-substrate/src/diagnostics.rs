//! Failure-only diagnostic bundles for opted-in harness executions.

use std::env;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::execute::ExecutionError;

const ARTIFACT_DIRECTORY: &str = "substrate-harness-artifacts";
const EXECUTION_DIRECTORY: &str = "execution";
const DIAGNOSTICS_FILE: &str = "diagnostics.json";

#[derive(Clone)]
pub struct CompletedStep {
    pub label: String,
    pub output_class: &'static str,
    pub byte_length: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistenceFault {
    None,
    Write,
    Publish,
}

#[derive(Serialize)]
struct DiagnosticsDocument<'a> {
    version: u8,
    id: &'a str,
    failure: FailureDocument<'a>,
    completed: Vec<CompletedStepDocument<'a>>,
    observed_kinds: Vec<String>,
}

#[derive(Serialize)]
struct FailureDocument<'a> {
    category: &'static str,
    message: String,
    failing_label: Option<&'a str>,
}

#[derive(Serialize)]
struct CompletedStepDocument<'a> {
    label: &'a str,
    output_class: &'static str,
    byte_length: usize,
}

#[allow(clippy::print_stderr)] // Artifact retention is secondary to the primary test failure.
pub fn write_failure(
    root: Option<&Path>,
    id: &str,
    completed: &[CompletedStep],
    observed_kinds: Vec<String>,
    error: &ExecutionError,
) {
    let root = root.map_or_else(artifact_root, Path::to_path_buf);
    if let Err(error) = write_failure_at(&root, id, completed, observed_kinds, error) {
        eprintln!("could not retain substrate harness diagnostics: {error}");
    }
}

#[cfg(test)]
#[allow(clippy::print_stderr)] // Artifact retention is secondary to the primary test failure.
pub fn write_failure_with_fault(
    root: Option<&Path>,
    id: &str,
    completed: &[CompletedStep],
    observed_kinds: Vec<String>,
    error: &ExecutionError,
    fault: PersistenceFault,
) {
    let root = root.map_or_else(artifact_root, Path::to_path_buf);
    if let Err(error) = write_failure_at_with_fault(&root, id, completed, observed_kinds, error, fault) {
        eprintln!("could not retain substrate harness diagnostics: {error}");
    }
}

#[allow(clippy::disallowed_methods)] // CARGO_TARGET_DIR is Cargo's external artifact-root contract.
fn artifact_root() -> PathBuf {
    env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from("target"), PathBuf::from).join(ARTIFACT_DIRECTORY)
}

fn write_failure_at(
    root: &Path,
    id: &str,
    completed: &[CompletedStep],
    observed_kinds: Vec<String>,
    error: &ExecutionError,
) -> Result<(), String> {
    write_failure_at_with_fault(root, id, completed, observed_kinds, error, PersistenceFault::None)
}

fn write_failure_at_with_fault(
    root: &Path,
    id: &str,
    completed: &[CompletedStep],
    observed_kinds: Vec<String>,
    error: &ExecutionError,
    fault: PersistenceFault,
) -> Result<(), String> {
    let leaf = sanitize_id(id);
    let execution_root = root.join(EXECUTION_DIRECTORY);
    fs::create_dir_all(&execution_root).map_err(|error| format!("create {}: {error}", execution_root.display()))?;

    let replacement = temporary_sibling(&execution_root, &leaf)?;
    fs::create_dir(&replacement).map_err(|error| format!("create {}: {error}", replacement.display()))?;
    let result = (|| {
        let document = document(id, completed, observed_kinds, error);
        let encoded =
            serde_json::to_vec_pretty(&document).map_err(|error| format!("serialize diagnostics: {error}"))?;
        write_atomically(&replacement.join(DIAGNOSTICS_FILE), &encoded, fault)?;

        let destination = execution_root.join(leaf);
        if fault == PersistenceFault::Publish {
            return Err("injected diagnostics publication failure".to_owned());
        }
        if destination.exists() {
            fs::remove_dir_all(&destination).map_err(|error| format!("replace {}: {error}", destination.display()))?;
        }
        fs::rename(&replacement, &destination).map_err(|error| format!("publish {}: {error}", destination.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&replacement);
    }
    result
}

fn document<'a>(
    id: &'a str,
    completed: &'a [CompletedStep],
    observed_kinds: Vec<String>,
    error: &'a ExecutionError,
) -> DiagnosticsDocument<'a> {
    DiagnosticsDocument {
        version: 1,
        id,
        failure: FailureDocument {
            category: error_category(error),
            message: error.to_string(),
            failing_label: Some(failure_label(error)),
        },
        completed: completed
            .iter()
            .map(|step| CompletedStepDocument {
                label: &step.label,
                output_class: step.output_class,
                byte_length: step.byte_length,
            })
            .collect(),
        observed_kinds,
    }
}

fn error_category(error: &ExecutionError) -> &'static str {
    match error {
        ExecutionError::DuplicateLabel(_) => "duplicate_label",
        ExecutionError::OpFailed { .. } => "operation_failed",
        ExecutionError::NoSuchReply(_) => "no_such_reply",
        ExecutionError::ReplyDecode { .. } => "reply_decode",
        ExecutionError::PollTimeout { .. } => "poll_timeout",
    }
}

fn failure_label(error: &ExecutionError) -> &str {
    match error {
        ExecutionError::DuplicateLabel(label)
        | ExecutionError::OpFailed { label, .. }
        | ExecutionError::ReplyDecode { label, .. }
        | ExecutionError::PollTimeout { label, .. }
        | ExecutionError::NoSuchReply(label) => label,
    }
}

fn sanitize_id(id: &str) -> String {
    let leaf: String = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let leaf = leaf.trim_matches('-');
    if leaf.is_empty() {
        "execution".to_owned()
    } else {
        leaf.to_owned()
    }
}

fn temporary_sibling(parent: &Path, leaf: &str) -> Result<PathBuf, String> {
    let nonce =
        SystemTime::now().duration_since(UNIX_EPOCH).map_err(|error| format!("read system clock: {error}"))?.as_nanos();
    Ok(parent.join(format!(".{leaf}.{nonce}.tmp")))
}

fn write_atomically(path: &Path, bytes: &[u8], fault: PersistenceFault) -> Result<(), String> {
    let temporary = path.with_extension("json.tmp");
    let mut file = fs::File::create(&temporary).map_err(|error| format!("create {}: {error}", temporary.display()))?;
    if fault == PersistenceFault::Write {
        return Err("injected diagnostics write failure".to_owned());
    }
    file.write_all(bytes).map_err(|error| format!("write {}: {error}", temporary.display()))?;
    file.write_all(b"\n").map_err(|error| format!("finish {}: {error}", temporary.display()))?;
    file.sync_all().map_err(|error| format!("sync {}: {error}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|error| format!("publish {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::env::temp_dir;
    use std::fs;
    use std::process::id;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::SubstrateHarnessError;

    fn failure() -> ExecutionError {
        ExecutionError::OpFailed {
            label: "fail".to_owned(),
            error: SubstrateHarnessError::UnknownMailbox("missing".to_owned()),
        }
    }

    fn temporary_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock").as_nanos();
        let path = temp_dir().join(format!("aether-harness-diagnostics-{name}-{}-{nonce}", id()));
        fs::create_dir(&path).expect("create temporary root");
        path
    }

    #[test]
    fn sanitizer_keeps_one_safe_leaf() {
        assert_eq!(sanitize_id("../../wrong leaf"), "wrong-leaf");
        assert_eq!(sanitize_id(""), "execution");
    }

    #[test]
    fn writes_bounded_deterministic_projection_without_payloads() {
        let root = temporary_root("projection");
        let completed = vec![CompletedStep { label: "reply".to_owned(), output_class: "replied", byte_length: 99 }];
        write_failure_at(&root, "one/two", &completed, vec!["aether.test".to_owned()], &failure())
            .expect("write diagnostics");
        let bytes = fs::read_to_string(root.join("execution/one-two/diagnostics.json")).expect("read diagnostics");
        let repeated = root.join("execution/one-two/diagnostics.json");
        write_failure_at(&root, "one/two", &completed, vec!["aether.test".to_owned()], &failure())
            .expect("replace diagnostics");
        assert_eq!(bytes, fs::read_to_string(repeated).expect("read repeated diagnostics"));
        assert!(bytes.contains("\"version\": 1"));
        assert!(bytes.contains("\"byte_length\": 99"));
        assert!(!bytes.contains("raw mail bytes"));
        fs::remove_dir_all(root).expect("remove temporary root");
    }

    #[test]
    fn replacing_one_execution_leaf_preserves_neighbors() {
        let root = temporary_root("replacement");
        let completed = vec![CompletedStep { label: "first".to_owned(), output_class: "mailed", byte_length: 0 }];
        write_failure_at(&root, "same", &completed, Vec::new(), &failure()).expect("first write");
        fs::write(root.join("execution/other"), "neighbor").expect("neighbor");
        write_failure_at(&root, "same", &[], Vec::new(), &failure()).expect("replacement write");
        let bytes = fs::read_to_string(root.join("execution/same/diagnostics.json")).expect("replacement document");
        assert!(!bytes.contains("first"));
        assert_eq!(fs::read_to_string(root.join("execution/other")).expect("neighbor remains"), "neighbor");
        fs::remove_dir_all(root).expect("remove temporary root");
    }

    #[test]
    fn error_projection_names_each_category_label() {
        let errors = [
            ExecutionError::DuplicateLabel("duplicate".to_owned()),
            failure(),
            ExecutionError::NoSuchReply("absent".to_owned()),
            ExecutionError::ReplyDecode { label: "decode".to_owned(), error: "bad".to_owned() },
            ExecutionError::PollTimeout {
                label: "poll".to_owned(),
                recipient: "mailbox".to_owned(),
                observed_kind: "test.kind",
                budget: Duration::ZERO,
                probes: 1,
                observed: "none".to_owned(),
            },
        ];
        assert_eq!(error_category(&errors[0]), "duplicate_label");
        assert_eq!(failure_label(&errors[0]), "duplicate");
        assert_eq!(error_category(&errors[1]), "operation_failed");
        assert_eq!(failure_label(&errors[1]), "fail");
        assert_eq!(error_category(&errors[2]), "no_such_reply");
        assert_eq!(failure_label(&errors[2]), "absent");
        assert_eq!(error_category(&errors[3]), "reply_decode");
        assert_eq!(failure_label(&errors[3]), "decode");
        assert_eq!(error_category(&errors[4]), "poll_timeout");
        assert_eq!(failure_label(&errors[4]), "poll");
    }

    fn fault_keeps_published_leaf_and_neighbors(fault: PersistenceFault) {
        let root = temporary_root("fault");
        let completed = vec![CompletedStep { label: "old".to_owned(), output_class: "mailed", byte_length: 0 }];
        write_failure_at(&root, "same", &completed, Vec::new(), &failure()).expect("publish prior leaf");
        fs::write(root.join("execution/neighbor"), "neighbor").expect("publish neighbor");
        let prior = fs::read_to_string(root.join("execution/same/diagnostics.json")).expect("read prior leaf");

        let error = write_failure_at_with_fault(&root, "same", &[], Vec::new(), &failure(), fault)
            .expect_err("injected persistence fault fails");
        assert!(error.contains("injected diagnostics"));
        assert_eq!(
            fs::read_to_string(root.join("execution/same/diagnostics.json")).expect("prior leaf remains"),
            prior
        );
        assert_eq!(fs::read_to_string(root.join("execution/neighbor")).expect("neighbor remains"), "neighbor");
        assert!(
            fs::read_dir(root.join("execution")).expect("list execution root").all(|entry| !entry
                .expect("execution entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
            "failed persistence leaves no temporary sibling"
        );
        fs::remove_dir_all(root).expect("remove temporary root");
    }

    #[test]
    fn write_fault_cleans_temporary_sibling_without_replacing_published_data() {
        fault_keeps_published_leaf_and_neighbors(PersistenceFault::Write);
    }

    #[test]
    fn publication_fault_cleans_temporary_sibling_without_replacing_published_data() {
        fault_keeps_published_leaf_and_neighbors(PersistenceFault::Publish);
    }
}
