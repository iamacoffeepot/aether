//! Incremental workpiece assembly (ADR-0208).
//!
//! The builder holds content; the records are its durable shadow.
//! [`WorkpieceBuilder`] keeps an append-only vector of [`WorkpieceFact`] in
//! emission order — byte-identical to what a store or a lane record file would
//! hold — and a map of the [`FieldEntry`] detail artifacts it minted along the
//! way. Every check and every projection reads content out of that map by the
//! record's `detail`.
//!
//! A builder rehydrated from records alone can still report presence and
//! emission order through [`WorkpieceFields`]. Resolving content is a
//! capability of the *in-flight* builder, and [`WorkpieceBuilder::finish`] is
//! an in-flight operation.
//!
//! Per-kind arity — which kinds are singular and which repeated — lives here,
//! not as a method on [`FieldKind`], so that vocabulary stays a frozen wire
//! type. [`FieldKind::InverseSearch`] and [`FieldKind::Implements`] have no
//! authoring setter in this slice: the first is issue 5300's derived field, and
//! the second needs an ADR digest rather than text.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::{
    FieldKind, SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting, SurfacePattern, WorkpieceFact, WorkpieceFields,
};
use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::WorkpieceId;

/// The schema number a version-1 [`FieldEntry`] writes into its first field.
pub const FIELD_ENTRY_SCHEMA: u32 = 1;

/// One content-addressed field-record payload (ADR-0208).
///
/// The generation lives here rather than on [`WorkpieceFact`]: that struct is
/// three fields permanently, and appending one would make every previously
/// persisted record undecodable.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FieldEntry {
    /// Schema version. Version 1 writes [`FIELD_ENTRY_SCHEMA`]. A later schema
    /// is a new encoding, not a mutation of this field's meaning.
    pub schema: u32,
    /// One setter call is one generation. Resolution keeps the highest.
    pub generation: u32,
    /// Position within that generation. A singular field writes slot 0; a
    /// repeated field writes one record per element in slot order.
    pub slot: u32,
    /// The field's authored text.
    pub text: String,
}

impl ContentAddressed for FieldEntry {
    const DOMAIN: &'static str = "aether.bloomery.workpiece_field_entry";
}

/// In-flight accumulation of a workpiece's typed field records (ADR-0208).
///
/// Setters are last-write-wins per [`FieldKind`], keyed by a generation carried
/// in the [`FieldEntry`] they mint. [`Self::records`] preserves every write in
/// emission order; resolution and [`Self::finish`] read only the winning
/// generation's content.
#[derive(Debug)]
pub struct WorkpieceBuilder {
    workpiece: WorkpieceId,
    records: Vec<WorkpieceFact>,
    content: BTreeMap<Digest, FieldEntry>,
    generations: BTreeMap<FieldKind, u32>,
}

/// Why [`WorkpieceBuilder::finish`] refused to project a [`ScopeRevision`].
///
/// Each variant names the offending slot and its text so refusals are
/// distinguishable by variant rather than by message. A repeated [`FieldKind`]
/// is not a violation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WorkpieceRefusal {
    /// The resolved workpiece carries no plan step.
    NoPlanStep {
        /// No plan record exists to name.
        slot: u32,
        /// Empty; there is no plan-step text to report.
        text: String,
    },
    /// The resolved workpiece carries no declared-surface glob.
    EmptyDeclaredSurface {
        /// No surface record exists to name.
        slot: u32,
        /// Empty; there is no glob to report.
        text: String,
    },
    /// A declared-surface glob is outside the grammar of [`SurfacePattern::parse`].
    InvalidSurface {
        /// The offending glob's slot in the winning generation.
        slot: u32,
        /// The glob [`SurfacePattern::parse`] rejected.
        text: String,
    },
    /// An edge names a blank workpiece id.
    BlankEdge {
        /// The offending edge's slot in the winning generation.
        slot: u32,
        /// The blank id text.
        text: String,
    },
    /// The resolved workpiece carries no problem statement.
    MissingProblem {
        /// No problem record exists to name.
        slot: u32,
        /// Empty; there is no problem text to report.
        text: String,
    },
    /// The problem statement is present and blank.
    BlankProblem {
        /// The problem record's slot (singular fields write slot 0).
        slot: u32,
        /// The blank problem text.
        text: String,
    },
}

impl WorkpieceBuilder {
    /// An empty in-flight builder for `workpiece`.
    #[must_use]
    pub fn new(workpiece: WorkpieceId) -> Self {
        Self { workpiece, records: Vec::new(), content: BTreeMap::new(), generations: BTreeMap::new() }
    }

    /// Field records in emission order, including overwritten generations.
    #[must_use]
    pub fn records(&self) -> &[WorkpieceFact] {
        &self.records
    }

    /// Emission-ordered projection over [`Self::records`].
    ///
    /// Presence and streaming do not resolve any detail artifact.
    #[must_use]
    pub fn fields(&self) -> WorkpieceFields {
        WorkpieceFields { workpiece: self.workpiece.clone(), facts: self.records.clone() }
    }

    /// The problem statement. Singular: one generation, slot 0.
    pub fn problem(&mut self, text: &str) -> &mut Self {
        self.set_singular(FieldKind::Problem, text)
    }

    /// What success looks like. Singular: one generation, slot 0.
    pub fn success(&mut self, text: &str) -> &mut Self {
        self.set_singular(FieldKind::Success, text)
    }

    /// The chosen approach. Singular: one generation, slot 0.
    pub fn approach(&mut self, text: &str) -> &mut Self {
        self.set_singular(FieldKind::Approach, text)
    }

    /// A routing property of the work. Singular: one generation, slot 0.
    pub fn routing_hint(&mut self, text: &str) -> &mut Self {
        self.set_singular(FieldKind::RoutingHint, text)
    }

    /// Evidence grounding the problem. Repeated: one generation, slot order.
    pub fn evidence<I, S>(&mut self, items: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.set_list(FieldKind::Evidence, items)
    }

    /// Options considered and rejected. Repeated: one generation, slot order.
    pub fn rejected_option<I, S>(&mut self, items: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.set_list(FieldKind::RejectedOption, items)
    }

    /// Implementation-plan steps. Repeated: one generation, slot order.
    pub fn plan_step<I, S>(&mut self, items: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.set_list(FieldKind::PlanStep, items)
    }

    /// Acceptance criteria. Repeated: one generation, slot order.
    pub fn acceptance<I, S>(&mut self, items: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.set_list(FieldKind::Acceptance, items)
    }

    /// Declared-surface globs. Repeated: one generation, slot order.
    pub fn declared_surface<I, S>(&mut self, items: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.set_list(FieldKind::DeclaredSurface, items)
    }

    /// Edges to other workpieces. Repeated: one generation, slot order.
    pub fn edge<I, S>(&mut self, items: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.set_list(FieldKind::Edge, items)
    }

    /// Project a coherent record set into a [`ScopeRevision`].
    ///
    /// `predecessor` is the commission's current tip, which is not a property
    /// of the workpiece. `routing` is supplied by the caller because
    /// [`FieldKind::RoutingHint`] is not an authored size or a model name;
    /// hint records render as advisory trailing bullets of `design`.
    ///
    /// # Errors
    ///
    /// [`WorkpieceRefusal`] when the resolved content cannot be a scope: no
    /// plan step, an empty declared surface, a glob outside the grammar, a
    /// blank edge id, a missing problem, or a blank problem, in that order.
    pub fn finish(
        &self,
        predecessor: Option<Digest>,
        routing: ScopeRouting,
    ) -> Result<ScopeRevision, WorkpieceRefusal> {
        if let Some(refusal) = self.refusal() {
            return Err(refusal);
        }
        Ok(self.render(predecessor, routing))
    }

    /// Mint a [`FieldEntry`], address it, store the content, and push one fact.
    ///
    /// Every setter goes through this funnel. Issue 5300 hooks derived-field
    /// production here, so no setter may write `records` directly.
    fn append(&mut self, kind: FieldKind, generation: u32, slot: u32, text: String) {
        let entry = FieldEntry { schema: FIELD_ENTRY_SCHEMA, generation, slot, text };
        let detail = digest_of(&entry);
        self.content.insert(detail, entry);
        self.records.push(WorkpieceFact { workpiece: self.workpiece.clone(), kind, detail });
    }

    fn bump(&mut self, kind: FieldKind) -> u32 {
        let generation = self.generations.get(&kind).copied().map_or(0, |current| current.saturating_add(1));
        self.generations.insert(kind, generation);
        generation
    }

    fn set_singular(&mut self, kind: FieldKind, text: &str) -> &mut Self {
        let generation = self.bump(kind);
        self.append(kind, generation, 0, String::from(text));
        self
    }

    fn set_list<I, S>(&mut self, kind: FieldKind, items: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let generation = self.bump(kind);
        let mut slot = 0;
        for item in items {
            self.append(kind, generation, slot, item.into());
            slot = slot.saturating_add(1);
        }
        self
    }

    /// Winning generation of `kind`, in slot order.
    ///
    /// Groups the append-ordered record vector by kind, takes the maximum
    /// generation, and yields that generation's entries without sorting the
    /// records or reading a clock. [`None`] is absent — unwritten, distinct
    /// from a kind written with empty text.
    fn resolved(&self, kind: FieldKind) -> Option<Vec<&FieldEntry>> {
        let generation = self.entries_of(kind).map(|entry| entry.generation).max()?;
        Some(self.entries_of(kind).filter(|entry| entry.generation == generation).collect())
    }

    fn entries_of(&self, kind: FieldKind) -> impl Iterator<Item = &FieldEntry> {
        self.records.iter().filter(move |fact| fact.kind == kind).filter_map(|fact| self.content.get(&fact.detail))
    }

    fn texts(&self, kind: FieldKind) -> Vec<&str> {
        self.resolved(kind).into_iter().flatten().map(|entry| entry.text.as_str()).collect()
    }

    fn refusal(&self) -> Option<WorkpieceRefusal> {
        if self.resolved(FieldKind::PlanStep).is_none() {
            return Some(WorkpieceRefusal::NoPlanStep { slot: 0, text: String::new() });
        }

        let Some(surface) = self.resolved(FieldKind::DeclaredSurface) else {
            return Some(WorkpieceRefusal::EmptyDeclaredSurface { slot: 0, text: String::new() });
        };
        if let Some(entry) = surface.iter().find(|entry| SurfacePattern::parse(&entry.text).is_none()) {
            return Some(WorkpieceRefusal::InvalidSurface { slot: entry.slot, text: entry.text.clone() });
        }

        if let Some(entry) =
            self.resolved(FieldKind::Edge).into_iter().flatten().find(|entry| entry.text.trim().is_empty())
        {
            return Some(WorkpieceRefusal::BlankEdge { slot: entry.slot, text: entry.text.clone() });
        }

        let Some(problem) = self.resolved(FieldKind::Problem) else {
            return Some(WorkpieceRefusal::MissingProblem { slot: 0, text: String::new() });
        };
        if let Some(entry) = problem.first()
            && entry.text.trim().is_empty()
        {
            return Some(WorkpieceRefusal::BlankProblem { slot: entry.slot, text: entry.text.clone() });
        }

        None
    }

    fn render(&self, predecessor: Option<Digest>, routing: ScopeRouting) -> ScopeRevision {
        ScopeRevision {
            schema: SCOPE_REVISION_SCHEMA,
            workpiece: self.workpiece.clone(),
            predecessor,
            problem: self.problem_paragraphs().join("\n\n"),
            design: self.design_sections().join("\n\n"),
            plan: self.render_plan(),
            declared_surface: owned_texts(self.texts(FieldKind::DeclaredSurface)),
            dogfood_brief: String::new(),
            routing,
            dependencies: self.texts(FieldKind::Edge).into_iter().map(|id| WorkpieceId(String::from(id))).collect(),
            description: String::new(),
            implements: Vec::new(),
        }
    }

    fn problem_paragraphs(&self) -> Vec<String> {
        [FieldKind::Problem, FieldKind::Evidence, FieldKind::Success]
            .into_iter()
            .flat_map(|kind| owned_texts(self.texts(kind)))
            .collect()
    }

    fn design_sections(&self) -> Vec<String> {
        let mut sections = Vec::new();
        if let Some(text) = self.texts(FieldKind::Approach).first() {
            sections.push(alloc::format!("### Chosen approach\n\n{text}"));
        }
        let rejected = self.texts(FieldKind::RejectedOption);
        if !rejected.is_empty() {
            let body = bullets(&rejected);
            sections.push(alloc::format!("### Rejected options\n\n{body}"));
        }
        let hints = self.texts(FieldKind::RoutingHint);
        if !hints.is_empty() {
            sections.push(bullets(&hints));
        }
        sections
    }

    fn render_plan(&self) -> String {
        self.texts(FieldKind::PlanStep)
            .into_iter()
            .enumerate()
            .map(|(index, text)| alloc::format!("{}. {text}", index + 1))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn owned_texts(texts: Vec<&str>) -> Vec<String> {
    texts.into_iter().map(String::from).collect()
}

fn bullets(texts: &[&str]) -> String {
    let mut out = String::new();
    for text in texts {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("- ");
        out.push_str(text);
    }
    out
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{
        FIELD_ENTRY_SCHEMA, FieldEntry, FieldKind, SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting,
        WorkpieceBuilder, WorkpieceRefusal,
    };
    use crate::digest::{Digest, digest_of};
    use crate::ids::WorkpieceId;

    fn workpiece() -> WorkpieceId {
        WorkpieceId(String::from("issue-5299"))
    }

    fn routing() -> ScopeRouting {
        ScopeRouting { size: String::from("l"), model: String::from("grok-4.6") }
    }

    fn required(builder: &mut WorkpieceBuilder) -> &mut WorkpieceBuilder {
        builder
            .problem("the problem")
            .plan_step(["do the work"])
            .declared_surface(["crates/aether-bloomery/src/lib.rs"])
    }

    fn generations_of(builder: &WorkpieceBuilder, kind: FieldKind) -> Vec<u32> {
        builder
            .records()
            .iter()
            .filter(|fact| fact.kind == kind)
            .map(|fact| builder.content.get(&fact.detail).expect("in-flight content").generation)
            .collect()
    }

    fn resolved_texts(builder: &WorkpieceBuilder, kind: FieldKind) -> Vec<&str> {
        builder.resolved(kind).into_iter().flatten().map(|entry| entry.text.as_str()).collect()
    }

    // Tripwire: content address of the FieldEntry fixture under
    // `aether.bloomery.workpiece_field_entry`. This golden is not repinnable.
    // A drift means the struct grew a field or the domain tag moved, and either
    // is a new type.
    const GOLDEN_FIELD_ENTRY_DIGEST: [u8; 32] = [
        88, 4, 135, 49, 163, 155, 251, 225, 196, 243, 126, 196, 218, 217, 57, 26, 144, 96, 144, 211, 27, 49, 126, 117,
        147, 139, 133, 204, 148, 100, 84, 20,
    ];

    fn field_entry_fixture() -> FieldEntry {
        FieldEntry { schema: FIELD_ENTRY_SCHEMA, generation: 0, slot: 0, text: String::from("problem") }
    }

    #[test]
    fn field_entry_digest_is_not_repinnable() {
        let digest = digest_of(&field_entry_fixture());
        assert_eq!(
            *digest.as_bytes(),
            GOLDEN_FIELD_ENTRY_DIGEST,
            "FieldEntry content addressing drifted; digest={digest:?}"
        );
    }

    #[test]
    fn restated_declared_surface_resolves_to_the_later_write() {
        let mut builder = WorkpieceBuilder::new(workpiece());
        builder.declared_surface(["crates/a/**", "crates/b/**"]);
        builder.declared_surface(["crates/c/**"]);

        assert_eq!(resolved_texts(&builder, FieldKind::DeclaredSurface), ["crates/c/**"]);
        assert_eq!(generations_of(&builder, FieldKind::DeclaredSurface), [0, 0, 1]);
        assert_eq!(builder.fields().records(FieldKind::DeclaredSurface).count(), 3);
        assert_eq!(builder.records().len(), 3);
    }

    #[test]
    fn narrowing_declared_surface_drops_the_losing_slots() {
        let mut builder = WorkpieceBuilder::new(workpiece());
        builder.declared_surface(["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"]);
        builder.declared_surface(["a.rs", "b.rs", "c.rs"]);

        assert_eq!(resolved_texts(&builder, FieldKind::DeclaredSurface), ["a.rs", "b.rs", "c.rs"]);
        assert_eq!(builder.records().len(), 8);
        assert_eq!(generations_of(&builder, FieldKind::DeclaredSurface), [0, 0, 0, 0, 0, 1, 1, 1]);
    }

    #[test]
    fn ten_setter_calls_across_five_kinds_project_to_five_fields() {
        let mut builder = WorkpieceBuilder::new(workpiece());
        builder
            .problem("first problem")
            .problem("second problem")
            .success("first success")
            .success("second success")
            .approach("first approach")
            .approach("second approach")
            .plan_step(["first step"])
            .plan_step(["second step"])
            .declared_surface(["one.rs"])
            .declared_surface(["two.rs"]);

        assert_eq!(builder.records().len(), 10);
        let resolved = [
            FieldKind::Problem,
            FieldKind::Success,
            FieldKind::Approach,
            FieldKind::PlanStep,
            FieldKind::DeclaredSurface,
        ]
        .into_iter()
        .filter(|kind| builder.resolved(*kind).is_some())
        .count();
        assert_eq!(resolved, 5);
        assert_eq!(resolved_texts(&builder, FieldKind::Problem), ["second problem"]);
        assert_eq!(resolved_texts(&builder, FieldKind::PlanStep), ["second step"]);
        assert_eq!(resolved_texts(&builder, FieldKind::DeclaredSurface), ["two.rs"]);
    }

    #[test]
    fn finish_refuses_a_blank_edge() {
        let mut builder = WorkpieceBuilder::new(workpiece());
        let err = required(&mut builder).edge([""]).finish(None, routing());
        assert!(matches!(err, Err(WorkpieceRefusal::BlankEdge { .. })));
    }

    #[test]
    fn finish_refuses_a_missing_plan_step() {
        let mut builder = WorkpieceBuilder::new(workpiece());
        builder.problem("the problem").declared_surface(["crates/aether-bloomery/src/lib.rs"]);
        assert!(matches!(builder.finish(None, routing()), Err(WorkpieceRefusal::NoPlanStep { .. })));
    }

    #[test]
    fn finish_refuses_an_empty_or_illegal_declared_surface() {
        let mut empty = WorkpieceBuilder::new(workpiece());
        empty.problem("the problem").plan_step(["do the work"]);
        assert!(matches!(empty.finish(None, routing()), Err(WorkpieceRefusal::EmptyDeclaredSurface { .. })));

        let mut illegal = WorkpieceBuilder::new(workpiece());
        illegal.problem("the problem").plan_step(["do the work"]).declared_surface(["*"]);
        assert!(matches!(illegal.finish(None, routing()), Err(WorkpieceRefusal::InvalidSurface { .. })));
    }

    #[test]
    fn finish_refuses_a_blank_or_missing_problem() {
        let mut blank = WorkpieceBuilder::new(workpiece());
        blank.problem("").plan_step(["do the work"]).declared_surface(["crates/aether-bloomery/src/lib.rs"]);
        assert!(matches!(blank.finish(None, routing()), Err(WorkpieceRefusal::BlankProblem { .. })));

        let mut missing = WorkpieceBuilder::new(workpiece());
        missing.plan_step(["do the work"]).declared_surface(["crates/aether-bloomery/src/lib.rs"]);
        assert!(matches!(missing.finish(None, routing()), Err(WorkpieceRefusal::MissingProblem { .. })));
    }

    #[test]
    fn finish_projects_a_passing_set_through_the_scope_revision_digest() {
        let predecessor = Digest::from_bytes([7; 32]);
        let mut builder = WorkpieceBuilder::new(workpiece());
        builder
            .problem("broken builder")
            .evidence(["nothing consumes WorkpieceFact"])
            .success("a checked ScopeRevision")
            .approach("the builder holds content")
            .rejected_option(["collapse every kind to one value"])
            .plan_step(["add workpiece_builder.rs", "re-export the new types"])
            .declared_surface([
                "crates/aether-bloomery/src/values/workpiece_builder.rs",
                "crates/aether-bloomery/src/values/mod.rs",
            ])
            .edge(["issue-5298"])
            .routing_hint("remaining judgement");

        let routing = routing();
        let revision = builder.finish(Some(predecessor), routing.clone()).expect("coherent record set");
        assert_eq!(revision.predecessor, Some(predecessor));
        assert_eq!(revision.plan, "1. add workpiece_builder.rs\n2. re-export the new types");

        let expected = ScopeRevision {
            schema: SCOPE_REVISION_SCHEMA,
            workpiece: workpiece(),
            predecessor: Some(predecessor),
            problem: String::from("broken builder\n\nnothing consumes WorkpieceFact\n\na checked ScopeRevision"),
            design: String::from(
                "### Chosen approach\n\nthe builder holds content\n\n### Rejected options\n\n- collapse every kind to one value\n\n- remaining judgement",
            ),
            plan: String::from("1. add workpiece_builder.rs\n2. re-export the new types"),
            declared_surface: vec![
                String::from("crates/aether-bloomery/src/values/workpiece_builder.rs"),
                String::from("crates/aether-bloomery/src/values/mod.rs"),
            ],
            dogfood_brief: String::new(),
            routing,
            dependencies: vec![WorkpieceId(String::from("issue-5298"))],
            description: String::new(),
            implements: Vec::new(),
        };
        assert_eq!(digest_of(&revision), digest_of(&expected));
    }

    #[test]
    fn unwritten_kind_is_absent_from_an_empty_text_write() {
        let mut builder = WorkpieceBuilder::new(workpiece());
        assert!(builder.resolved(FieldKind::Problem).is_none());
        builder.problem("");
        let resolved = builder.resolved(FieldKind::Problem).expect("empty text is present");
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].text.is_empty());
    }
}
