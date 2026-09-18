#![allow(unused_imports)]

use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Env, Program, Pure, program};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.receiver.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.receiver.result")]
struct Out {
    n: u32,
}

struct ReceiverProg;

#[program]
impl Program for ReceiverProg {
    const NAME: &'static str = "test.program.receiver";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fails because run takes a receiver.";
    type Input = In;
    type Result = Out;

    fn run(&self, input: Self::Input, _env: &mut Env<Pure>) -> Result<Self::Result, Refusal> {
        Ok(input)
    }
}

fn main() {}
