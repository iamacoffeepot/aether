//! Declarative `Script` — name + ordered list of steps the runner
//! executes against a `TestBench`. Parses from YAML so scenarios live
//! alongside the component they cover as plain text the component
//! author edits without touching Rust.

use serde::{Deserialize, Serialize};

/// A complete scenario script. The `name` is surfaced in `RunReport`
/// so failure logs identify which script tripped without re-reading
/// the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Script {
    pub name: String,
    pub steps: Vec<Step>,
}

/// One operation the runner executes. v1 covers chassis control
/// (`Advance`, `Capture`, `Assert`) and component driving
/// (`LoadComponent`, `SendMail`); the latter pair routes through the
/// `KindCatalog` so YAML strings + values turn into wire bytes the
/// chassis's mail dispatcher consumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {
    /// Run `ticks` advance cycles. The chassis fans `Tick` to every
    /// subscribed component and drains the mail queue between each.
    Advance { ticks: u32 },
    /// Capture the current offscreen frame as a PNG and stash it on
    /// the runner so subsequent `Assert` steps can inspect it.
    Capture,
    /// Run an assertion against the bench state — visual checks
    /// inspect the most-recent capture; mail checks inspect the
    /// observation log accumulated since boot. Visual checks fail
    /// with a clear error if no capture has been taken yet.
    Assert { check: Check },
    /// Load a WASM component from `path` (filesystem, read by the
    /// runner before sending). `name` is optional; when omitted the
    /// substrate auto-derives one from the component's manifest.
    /// Fire-and-forget — the script doesn't observe the resulting
    /// `LoadResult`. A future step kind can add reply-correlation if
    /// scenarios need to gate on load success.
    LoadComponent {
        path: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Generic mail send. `recipient` is a mailbox name (e.g.
    /// `"aether.sink.io"`); `kind` names the payload kind in the
    /// substrate's descriptor inventory (e.g. `"aether.io.write"`);
    /// `params` is the YAML body the schema encoder maps onto the
    /// kind's wire shape.
    SendMail {
        recipient: String,
        kind: String,
        #[serde(default)]
        params: serde_yml::Value,
    },
}

/// Assertions runnable inside an `Assert` step. Visual variants
/// inspect the most-recent capture; mail variants inspect the bench's
/// observation log. Names describe the property being asserted, not
/// the failure mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Check {
    /// Visual: asserts at least one pixel in the frame has a non-zero
    /// RGB component. Weak — the chassis clear color is non-black, so
    /// this only catches "the GPU produced no output at all." Prefer
    /// `DiffersFromBackground` for "geometry rendered on top of the
    /// clear pass" checks.
    NotAllBlack,
    /// Visual: asserts at least one pixel differs from the top-left
    /// pixel by more than `tolerance` per channel. The top-left is
    /// (almost) always the chassis clear color in our scenes since
    /// geometry is centered, so this catches "the frame is a uniform
    /// clear color" — i.e. nothing rendered on top. `tolerance`
    /// absorbs sRGB-encoding noise across GPUs.
    DiffersFromBackground {
        #[serde(default = "default_tolerance")]
        tolerance: u8,
    },
    /// Mail: asserts at least `min_count` mail frames with kind name
    /// `name` were observed. Observations come from the bench's
    /// chassis-owned `aether.sink.render` sink (which receives
    /// `aether.draw_triangle` and `aether.camera` post-ADR-0074
    /// §Decision 7) plus broadcast / session-zero frames on the
    /// loopback — see `TestBench::count_observed` for the full
    /// surface.
    MailObserved {
        name: String,
        #[serde(default = "default_min_count")]
        min_count: usize,
    },
    /// Mail: asserts no mail frame with kind name `name` has been
    /// observed since bench boot. Negative companion to
    /// `MailObserved`. Bench observations are cumulative — if a kind
    /// arrived earlier in the script, this assert will fail even
    /// after the producing component has been dropped.
    MailNotObserved { name: String },
}

fn default_tolerance() -> u8 {
    5
}

fn default_min_count() -> usize {
    1
}

/// Parse a script from YAML source. Returns the underlying serde_yml
/// error wrapped with file context handled at call sites.
pub fn parse_script(yaml: &str) -> Result<Script, serde_yml::Error> {
    serde_yml::from_str(yaml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_basic_script() {
        let yaml = r#"
name: empty boot
steps:
  - op: advance
    ticks: 1
  - op: capture
  - op: assert
    check:
      kind: not_all_black
"#;
        let script = parse_script(yaml).expect("parse");
        assert_eq!(script.name, "empty boot");
        assert_eq!(script.steps.len(), 3);
        match &script.steps[0] {
            Step::Advance { ticks } => assert_eq!(*ticks, 1),
            other => panic!("expected Advance, got {other:?}"),
        }
        match &script.steps[1] {
            Step::Capture => (),
            other => panic!("expected Capture, got {other:?}"),
        }
        match &script.steps[2] {
            Step::Assert { check } => {
                assert!(matches!(check, Check::NotAllBlack));
            }
            other => panic!("expected Assert, got {other:?}"),
        }
    }

    #[test]
    fn parses_differs_from_background_with_default_tolerance() {
        let yaml = r#"
name: bg
steps:
  - op: assert
    check:
      kind: differs_from_background
"#;
        let script = parse_script(yaml).expect("parse");
        match &script.steps[0] {
            Step::Assert { check } => match check {
                Check::DiffersFromBackground { tolerance } => assert_eq!(*tolerance, 5),
                other => panic!("expected DiffersFromBackground, got {other:?}"),
            },
            other => panic!("expected Assert, got {other:?}"),
        }
    }

    #[test]
    fn parses_mail_observed_with_default_min_count() {
        let yaml = r#"
name: mail
steps:
  - op: assert
    check:
      kind: mail_observed
      name: aether.draw_triangle
"#;
        let script = parse_script(yaml).expect("parse");
        match &script.steps[0] {
            Step::Assert { check } => match check {
                Check::MailObserved { name, min_count } => {
                    assert_eq!(name, "aether.draw_triangle");
                    assert_eq!(*min_count, 1);
                }
                other => panic!("expected MailObserved, got {other:?}"),
            },
            other => panic!("expected Assert, got {other:?}"),
        }
    }

    #[test]
    fn parses_mail_not_observed() {
        let yaml = r#"
name: no-mail
steps:
  - op: assert
    check:
      kind: mail_not_observed
      name: aether.audio.note_on
"#;
        let script = parse_script(yaml).expect("parse");
        match &script.steps[0] {
            Step::Assert { check } => match check {
                Check::MailNotObserved { name } => {
                    assert_eq!(name, "aether.audio.note_on");
                }
                other => panic!("expected MailNotObserved, got {other:?}"),
            },
            other => panic!("expected Assert, got {other:?}"),
        }
    }

    #[test]
    fn empty_steps_is_valid() {
        let script = parse_script("name: nothing\nsteps: []\n").expect("parse");
        assert_eq!(script.name, "nothing");
        assert!(script.steps.is_empty());
    }

    #[test]
    fn missing_op_field_errors() {
        let yaml = "name: bad\nsteps:\n  - ticks: 1\n";
        assert!(parse_script(yaml).is_err());
    }

    #[test]
    fn parses_load_component_and_send_mail_steps() {
        let yaml = r#"
name: drive a component
steps:
  - op: load_component
    path: /tmp/mesh-viewer.wasm
    name: mv
  - op: send_mail
    recipient: aether.sink.io
    kind: aether.io.write
    params:
      namespace: save
      path: greeting.txt
      bytes: [104, 105]
"#;
        let script = parse_script(yaml).expect("parse");
        assert_eq!(script.steps.len(), 2);
        match &script.steps[0] {
            Step::LoadComponent { path, name } => {
                assert_eq!(path, "/tmp/mesh-viewer.wasm");
                assert_eq!(name.as_deref(), Some("mv"));
            }
            other => panic!("expected LoadComponent, got {other:?}"),
        }
        match &script.steps[1] {
            Step::SendMail {
                recipient,
                kind,
                params,
            } => {
                assert_eq!(recipient, "aether.sink.io");
                assert_eq!(kind, "aether.io.write");
                let mapping = params.as_mapping().expect("params is mapping");
                assert_eq!(
                    mapping.get(serde_yml::Value::String("namespace".to_owned())),
                    Some(&serde_yml::Value::String("save".to_owned()))
                );
            }
            other => panic!("expected SendMail, got {other:?}"),
        }
    }

    #[test]
    fn load_component_name_defaults_to_none() {
        let yaml = r#"
name: just-load
steps:
  - op: load_component
    path: /tmp/x.wasm
"#;
        let script = parse_script(yaml).expect("parse");
        match &script.steps[0] {
            Step::LoadComponent { name, .. } => assert!(name.is_none()),
            other => panic!("expected LoadComponent, got {other:?}"),
        }
    }
}
