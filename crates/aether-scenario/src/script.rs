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
    /// Run a visual assertion against the most recent capture. Fails
    /// the step if no capture has been taken yet, with a clear error
    /// pointing at script ordering.
    Assert { check: VisualAssert },
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

/// Visual assertions over a captured frame's decoded pixels. Names
/// describe the property being asserted, not the failure mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VisualAssert {
    /// Asserts at least one pixel in the frame is non-black. Cheapest
    /// "the chassis rendered something" check — useful when a script
    /// loaded a mesh and expects geometry to land on the framebuffer.
    NotAllBlack,
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
                assert!(matches!(check, VisualAssert::NotAllBlack));
            }
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
