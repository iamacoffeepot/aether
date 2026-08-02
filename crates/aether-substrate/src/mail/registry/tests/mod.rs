//! Tests for the mail registry, one sibling per production module.
//!
//! Each file is named for the module whose behaviour it exercises:
//! [`register`], [`resolve`], [`kinds`], [`publish`], [`inventory`],
//! [`birth`], [`alias`], [`apply`], [`staged`] and [`commands`] mirror
//! the `mailbox` siblings, while [`dispatch`], [`handlers`] and
//! [`relay`] mirror the registry-level ones. [`support`] holds the
//! fixtures more than one of them shares.

mod alias;
mod apply;
mod birth;
mod commands;
mod dispatch;
mod handlers;
mod inventory;
mod kinds;
mod publish;
mod register;
mod relay;
mod resolve;
mod staged;
mod support;
