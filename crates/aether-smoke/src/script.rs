//! Declarative `Script` — name + ordered list of steps the runner
//! executes against a `TestBench`. Parses from YAML so smoke tests
//! live alongside the component they cover as plain text the
//! component author edits without touching Rust.

use serde::{Deserialize, Serialize};

/// A complete smoke script. The `name` is surfaced in `RunReport`
/// so failure logs identify which script tripped without re-reading
/// the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Script {
    pub name: String,
    pub steps: Vec<Step>,
}

/// One operation the runner executes. The variants are intentionally
/// minimal in v1 — chassis-level smokes only. Component-driving
/// steps (`LoadComponent`, `SendMail`) join in a follow-up PR.
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
}
