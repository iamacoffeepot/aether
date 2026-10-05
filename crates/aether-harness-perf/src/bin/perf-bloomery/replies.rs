//! The stub vendor's replies: a stateless function of the request body.
//!
//! A turn's index is the count of `function_call` items its conversation
//! replays divided by the calls each turn asks for. Before the last turn the
//! reply asks for that many calls, rotating over the configured tools; the
//! last turn asks one `muse-end` call with `Done` arguments, so every session
//! rests `Completed` within its turn limit. Each session's conversation is its
//! own, so the replies need no state to serve several sessions at once.

use std::thread;
use std::time::Duration;

use aether_harness_bloomery::StubRequest;
use serde_json::{Value, json};

use crate::knobs::{Knobs, Tool};
use crate::seed::Probe;

/// Everything a reply depends on besides the request.
pub struct Script {
    turns: usize,
    calls: usize,
    tools: Vec<Tool>,
    probe: Probe,
    vendor_delay_millis: u64,
}

impl Script {
    pub fn new(knobs: &Knobs, probe: Probe) -> Self {
        let turns = usize::try_from(knobs.turns).unwrap_or(usize::MAX);
        Self {
            turns,
            calls: knobs.calls,
            tools: knobs.tools.clone(),
            probe,
            vendor_delay_millis: knobs.vendor_delay_millis,
        }
    }

    /// The responses-API body answering `request`, after holding the reply
    /// for the configured vendor delay.
    pub fn reply(&self, request: &StubRequest) -> Vec<u8> {
        if self.vendor_delay_millis > 0 {
            thread::sleep(Duration::from_millis(self.vendor_delay_millis));
        }
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let replayed = body["input"]
            .as_array()
            .map_or(0, |items| items.iter().filter(|item| item["type"] == "function_call").count());
        let turn = replayed / self.calls;

        let output = if turn + 1 < self.turns {
            (0..self.calls).map(|call| self.call(turn, call)).collect()
        } else {
            vec![Self::end(turn)]
        };
        let reply = json!({
            "id": format!("resp_{turn}"),
            "object": "response",
            "created_at": 1_790_000_000,
            "status": "completed",
            "error": null,
            "incomplete_details": null,
            "model": "muse-bench",
            "store": false,
            "output": output,
            "usage": {
                "input_tokens": 100,
                "input_tokens_details": { "cached_tokens": 0 },
                "output_tokens": 10,
                "output_tokens_details": { "reasoning_tokens": 0 },
                "total_tokens": 110,
            },
        });
        serde_json::to_vec(&reply).unwrap_or_default()
    }

    /// The last turn's `muse-end` call, ending the run done.
    fn end(turn: usize) -> Value {
        json!({
            "id": format!("fc_{turn}_end"),
            "type": "function_call",
            "status": "completed",
            "call_id": format!("call_t{turn}_end"),
            "name": "muse-end",
            "arguments": r#"{"ending": "Done", "text": "Done."}"#,
        })
    }

    /// Call `call` of turn `turn`, naming the tool its position rotates to.
    fn call(&self, turn: usize, call: usize) -> Value {
        let tool = self.tools[call % self.tools.len()];
        let arguments = match tool {
            Tool::Write => {
                json!({ "path": format!("bench/t{turn}c{call}.txt"), "text": format!("turn {turn} call {call}\n") })
            }
            Tool::List => json!({}),
            Tool::Read => json!({ "path": self.probe.read }),
            Tool::Grep => json!({ "pattern": self.probe.grep }),
        };
        json!({
            "id": format!("fc_{turn}_{call}"),
            "type": "function_call",
            "status": "completed",
            "call_id": format!("call_t{turn}_c{call}"),
            "name": tool.function(),
            "arguments": arguments.to_string(),
        })
    }
}
