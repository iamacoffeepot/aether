//! The `Artifact` job: one artifact as JSON, with the artifacts it cites
//! resolved inline level by level down to the requested depth.
//!
//! The job is sans-io: replies go in and a [`Step`] comes out, which the
//! actor performs. Nothing recurses over the artifact graph. Each read is
//! verified and decoded into a per-request arena of nodes, the digests a
//! level cites are read together once the level is done (each digest once
//! per request), and a final top-down splice copies each resolved node into
//! its parent under the reply's value budget.

use std::collections::{HashMap, HashSet, VecDeque};
use std::mem;

use aether_bloomery_kinds::{DeclarationsResult, ReadArtifactResult};
use aether_data::{Digest, KindId};
use serde_json::{Map, Value};

use super::kinds::{InspectArtifactResult, MAX_ARTIFACTS, MAX_DEPTH, MAX_VALUES};
use super::resolve::{Resolver, count_values, render};

/// What the actor performs next for one job.
pub enum Step {
    /// Read each digest from the journal.
    Read(Vec<Digest>),
    /// Ask the driver for its program declarations.
    Declarations,
    /// Answer the request; the job is done.
    Answer(InspectArtifactResult),
    /// Wait for a reply already asked for.
    Wait,
}

/// A verified payload waiting to be decoded.
struct Arrived {
    digest: Digest,
    level: u32,
    kind: KindId,
    payload: Vec<u8>,
}

/// One decoded artifact in the arena.
struct Node {
    kind: Option<String>,
    kind_id: KindId,
    json: Value,
    /// How many JSON values `json` holds.
    size: usize,
    /// Each digest `json` holds, as a pointer into it.
    refs: Vec<(String, Digest)>,
}

/// One `InspectArtifact` in flight.
pub struct ArtifactJob {
    root: Digest,
    depth: u32,
    resolver: Resolver,
    /// Whether the driver's declarations have been asked for.
    asked: bool,
    nodes: Vec<Node>,
    index: HashMap<Digest, usize>,
    /// Every digest read or queued, so each is read once.
    seen: HashSet<Digest>,
    /// Reads in flight, each at its level.
    reading: HashMap<Digest, u32>,
    arrived: VecDeque<Arrived>,
    /// Digests the level being decoded cites, read once it is done.
    next: Vec<(Digest, u32)>,
    /// JSON values decoded so far.
    values: usize,
    /// Cited artifacts queued for reading.
    queued: usize,
    truncated: bool,
}

impl ArtifactJob {
    /// Start reading `digest`, or refuse a `depth` past [`MAX_DEPTH`].
    pub fn start(digest: Digest, depth: u32) -> (Self, Step) {
        let mut job = Self {
            root: digest,
            depth,
            resolver: Resolver::default(),
            asked: false,
            nodes: Vec::new(),
            index: HashMap::new(),
            seen: HashSet::from([digest]),
            reading: HashMap::from([(digest, 0)]),
            arrived: VecDeque::new(),
            next: Vec::new(),
            values: 0,
            queued: 0,
            truncated: false,
        };
        if depth > MAX_DEPTH {
            job.reading.clear();
            let message = format!("depth {depth} is past the most an inspect resolves, {MAX_DEPTH}");
            return (job, Step::Answer(InspectArtifactResult::Err { message }));
        }
        (job, Step::Read(vec![digest]))
    }

    /// Feed the journal's answer for `digest`. The root's failure answers the
    /// request; a cited artifact that is missing, fails to read, or does not
    /// hash to its digest stays hex.
    pub fn on_read(&mut self, digest: Digest, result: ReadArtifactResult) -> Step {
        let Some(level) = self.reading.remove(&digest) else {
            return Step::Wait;
        };
        let root = level == 0;
        match result {
            ReadArtifactResult::Found { artifact } => match artifact.load(digest) {
                Ok(payload) => self.arrived.push_back(Arrived { digest, level, kind: artifact.kind(), payload }),
                Err(_) if root => {
                    let message = format!("the artifact stored at {digest} does not hash to its digest");
                    return Step::Answer(InspectArtifactResult::Err { message });
                }
                Err(_) => {}
            },
            ReadArtifactResult::Missing { .. } if root => {
                return Step::Answer(InspectArtifactResult::Missing { digest: *digest.as_bytes() });
            }
            ReadArtifactResult::Err { message, .. } if root => {
                return Step::Answer(InspectArtifactResult::Err { message });
            }
            ReadArtifactResult::Missing { .. } | ReadArtifactResult::Err { .. } => {}
        }
        self.advance()
    }

    /// Feed the driver's declarations.
    pub fn on_declarations(&mut self, result: DeclarationsResult) -> Step {
        self.resolver.declare(result);
        self.advance()
    }

    /// Decode every arrived payload, then read the next level or answer.
    fn advance(&mut self) -> Step {
        while let Some(arrived) = self.arrived.pop_front() {
            let Some(resolved) = self.resolver.resolve(arrived.kind) else {
                self.arrived.push_front(arrived);
                if self.asked {
                    return Step::Wait;
                }
                self.asked = true;
                return Step::Declarations;
            };
            let root = arrived.level == 0;
            if !root && self.values >= MAX_VALUES {
                self.truncated = true;
                continue;
            }

            let rendered = render(arrived.kind, &arrived.payload, &resolved, MAX_VALUES - self.values);
            if !root && rendered.over_budget {
                self.truncated = true;
                continue;
            }
            self.values += rendered.values;
            self.truncated |= rendered.truncated;

            if arrived.level < self.depth {
                for (_, digest) in &rendered.digests {
                    if self.seen.contains(digest) {
                        continue;
                    }
                    if self.queued >= MAX_ARTIFACTS {
                        self.truncated = true;
                        continue;
                    }
                    self.seen.insert(*digest);
                    self.queued += 1;
                    self.next.push((*digest, arrived.level + 1));
                }
            }
            self.index.insert(arrived.digest, self.nodes.len());
            self.nodes.push(Node {
                kind: resolved.name(),
                kind_id: arrived.kind,
                size: count_values(&rendered.json),
                json: rendered.json,
                refs: rendered.digests,
            });
        }

        if !self.reading.is_empty() {
            return Step::Wait;
        }
        if !self.next.is_empty() {
            let next = mem::take(&mut self.next);
            let digests = next.iter().map(|(digest, _)| *digest).collect();
            self.reading.extend(next);
            return Step::Read(digests);
        }
        Step::Answer(self.splice())
    }

    /// Build the reply: copy the root, then replace each resolved digest
    /// with `{digest, kind, value}`, level by level from the top, while the
    /// reply stays within [`MAX_VALUES`].
    fn splice(&mut self) -> InspectArtifactResult {
        let Some(&root) = self.index.get(&self.root) else {
            return InspectArtifactResult::Err { message: format!("the artifact at {} did not decode", self.root) };
        };
        let mut json = self.nodes[root].json.clone();
        let mut budget = MAX_VALUES.saturating_sub(self.nodes[root].size);
        let mut pending = VecDeque::from([(String::new(), root, 0)]);
        while let Some((prefix, node, level)) = pending.pop_front() {
            if level >= self.depth {
                continue;
            }
            for (pointer, digest) in &self.nodes[node].refs {
                let Some(&child) = self.index.get(digest) else {
                    continue;
                };
                // The wrapper object, its digest, and its kind, beside the value.
                let cost = self.nodes[child].size + 3;
                if cost > budget {
                    self.truncated = true;
                    continue;
                }
                let at = format!("{prefix}{pointer}");
                let Some(slot) = json.pointer_mut(&at) else {
                    continue;
                };
                budget -= cost;
                *slot = wrap(*digest, &self.nodes[child]);
                pending.push_back((format!("{at}/value"), child, level + 1));
            }
        }
        InspectArtifactResult::Found {
            kind_id: self.nodes[root].kind_id.0,
            kind: self.nodes[root].kind.clone(),
            json: json.to_string(),
            truncated: self.truncated,
        }
    }
}

/// A resolved digest as it replaces its hex: `{digest, kind, value}`.
fn wrap(digest: Digest, node: &Node) -> Value {
    let mut object = Map::new();
    object.insert("digest".to_owned(), Value::String(digest.to_string()));
    object.insert("kind".to_owned(), node.kind.clone().map_or(Value::Null, Value::String));
    object.insert("value".to_owned(), node.json.clone());
    Value::Object(object)
}
