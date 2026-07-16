//! An in-process fake GitHub (#3459 step 4).
//!
//! Models the projection's object store — issues and their comments — with
//! enough fidelity to drive the projection end-to-end with no token and no
//! network: create returns a fresh number, find scans marker keys exactly as
//! the real client does, update overwrites in place, and [`delete_issue`]
//! models an operator deleting a projection so the rebuild property (delete →
//! reappear) is exercisable.
//!
//! Compiled for this crate's own tests unconditionally and, behind the
//! `testing` feature, exported so the host demo (#3459 step 7) drives the same
//! double.
//!
//! [`delete_issue`]: FakeGithub::delete_issue

// The fake holds its `Mutex` guard to the end of each short method rather than
// dropping it a line early: this is an in-memory test double with no
// contention, so the nursery lint's early-drop rewrite buys nothing and only
// clutters the store methods.
#![allow(clippy::significant_drop_tightening)]

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::client::{Comment, GithubApi, GithubError, Issue, NewComment, NewIssue};
use crate::marker::parse_marker;

#[derive(Clone)]
struct StoredIssue {
    number: u64,
    title: String,
    body: String,
}

#[derive(Clone)]
struct StoredComment {
    id: u64,
    issue_number: u64,
    body: String,
}

#[derive(Default)]
struct State {
    next_issue: u64,
    next_comment: u64,
    issues: Vec<StoredIssue>,
    comments: Vec<StoredComment>,
}

/// An in-memory GitHub double implementing [`GithubApi`].
///
/// Cloning shares one backing store (an `Arc<Mutex<…>>`), so a demo can hand a
/// clone to a [`GithubProjection`](crate::GithubProjection) and keep another to
/// introspect the objects the projection created.
#[derive(Default, Clone)]
pub struct FakeGithub {
    state: Arc<Mutex<State>>,
}

impl FakeGithub {
    /// A fresh, empty fake.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many issues currently exist — the carbon-copy count a demo asserts.
    #[must_use]
    pub fn issue_count(&self) -> usize {
        self.lock().issues.len()
    }

    /// How many comments currently exist across all issues.
    #[must_use]
    pub fn comment_count(&self) -> usize {
        self.lock().comments.len()
    }

    /// The current issue numbers, ascending.
    #[must_use]
    pub fn issue_numbers(&self) -> Vec<u64> {
        let mut numbers: Vec<u64> = self.lock().issues.iter().map(|issue| issue.number).collect();
        numbers.sort_unstable();
        numbers
    }

    /// The current body of issue `number`, if it exists.
    #[must_use]
    pub fn issue_body(&self, number: u64) -> Option<String> {
        self.lock().issues.iter().find(|issue| issue.number == number).map(|issue| issue.body.clone())
    }

    /// Delete issue `number` and its comments — an operator removing a
    /// projection. The next reconcile finds no marker and recreates it.
    pub fn delete_issue(&self, number: u64) {
        let mut state = self.lock();
        state.issues.retain(|issue| issue.number != number);
        state.comments.retain(|comment| comment.issue_number != number);
    }
}

impl GithubApi for FakeGithub {
    fn find_issue(&self, key: &str) -> Result<Option<Issue>, GithubError> {
        let state = self.lock();
        Ok(state.issues.iter().find_map(|issue| {
            let marker = parse_marker(&issue.body);
            match &marker {
                Some(m) if m.key == key => {
                    Some(Issue { number: issue.number, title: issue.title.clone(), body: issue.body.clone(), marker })
                }
                _ => None,
            }
        }))
    }

    fn create_issue(&self, new: &NewIssue) -> Result<Issue, GithubError> {
        let mut state = self.lock();
        state.next_issue += 1;
        let number = state.next_issue;
        state.issues.push(StoredIssue { number, title: new.title.clone(), body: new.body.clone() });
        Ok(Issue { number, title: new.title.clone(), body: new.body.clone(), marker: parse_marker(&new.body) })
    }

    fn update_issue(&self, number: u64, title: &str, body: &str) -> Result<(), GithubError> {
        let mut state = self.lock();
        let Some(issue) = state.issues.iter_mut().find(|issue| issue.number == number) else {
            return Err(GithubError::Status { status: 404, body: format!("no issue {number}") });
        };
        title.clone_into(&mut issue.title);
        body.clone_into(&mut issue.body);
        Ok(())
    }

    fn find_comment(&self, issue_number: u64, key: &str) -> Result<Option<Comment>, GithubError> {
        let state = self.lock();
        Ok(state.comments.iter().filter(|comment| comment.issue_number == issue_number).find_map(|comment| {
            let marker = parse_marker(&comment.body);
            match &marker {
                Some(m) if m.key == key => Some(Comment { id: comment.id, body: comment.body.clone(), marker }),
                _ => None,
            }
        }))
    }

    fn create_comment(&self, new: &NewComment) -> Result<Comment, GithubError> {
        let mut state = self.lock();
        state.next_comment += 1;
        let id = state.next_comment;
        state.comments.push(StoredComment { id, issue_number: new.issue_number, body: new.body.clone() });
        Ok(Comment { id, body: new.body.clone(), marker: parse_marker(&new.body) })
    }

    fn update_comment(&self, comment_id: u64, body: &str) -> Result<(), GithubError> {
        let mut state = self.lock();
        let Some(comment) = state.comments.iter_mut().find(|comment| comment.id == comment_id) else {
            return Err(GithubError::Status { status: 404, body: format!("no comment {comment_id}") });
        };
        body.clone_into(&mut comment.body);
        Ok(())
    }
}
