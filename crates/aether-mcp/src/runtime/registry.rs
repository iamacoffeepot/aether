//! The tool and resource-provider registries.
//!
//! Both follow the trust shape `aether.http.server`'s route table already uses:
//! the registrant is the host-stamped `Source` of the inbound registration, not
//! a field in it, so a claim cannot be forged and the operation is gated to
//! in-process actors by construction. What is new here is what a *descriptor*
//! adds over a route key — a public name, a schema contract, and a catalog a
//! client caches — and every rule below exists because one of those three has a
//! failure mode a route does not.
//!
//! The registries hold no substrate types. Liveness and capability acceptance
//! reach them as predicates, so every decision in this file is exercisable
//! without booting a chassis, which is the whole reason the decisions live here
//! rather than inline in the actor.

use std::collections::BTreeMap;

use aether_data::canonical::kind_id_from_parts;
use aether_data::{KindId, MailboxId, Schema, SchemaType, wire};
use serde_json::Value;

use crate::kinds::{
    AddressedOutput, RegisterResourceProviderResult, RegisterResourceProviderSelf, RegisterToolResult,
    RegisterToolSelf, ResourceDescriptor, ToolAnnotations,
};
use crate::protocol::resources::{RESPONSE_RESOURCE_PREFIX, list_resources_result, normalize_provider_prefix};
use crate::protocol::tools::{ToolDescriptor, list_tools_result};
use crate::schema::{SchemaBudget, translate_tool_schema};

/// Longest accepted tool name, in bytes — the grammar's `{0,63}` tail plus its
/// leading character.
pub const TOOL_NAME_MAXIMUM_BYTES: usize = 64;
/// Bytes a tool description may carry.
pub const TOOL_DESCRIPTION_MAXIMUM_BYTES: usize = 4_096;
/// Bytes a tool title may carry.
pub const TOOL_TITLE_MAXIMUM_BYTES: usize = 256;

/// The registry ceilings, resolved from [`McpServerConfiguration`].
///
/// [`McpServerConfiguration`]: crate::McpServerConfiguration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryLimits {
    /// Tool descriptors the catalog will admit.
    pub maximum_registered_tools: usize,
    /// Discoverable resource descriptors the catalog will admit.
    pub maximum_discoverable_resources: usize,
    /// Bytes accepted in any one serialized schema carrier.
    pub maximum_schema_bytes: usize,
    /// Bytes the rendered listing may reach.
    pub maximum_http_response_bytes: usize,
    /// Depth and node bounds for both schema walks.
    pub schema_budget: SchemaBudget,
}

/// A set of interchangeable holders with its own round-robin cursor.
///
/// Shared by both registries so "join, leave, pick the next live one" is
/// written once. Selection *skips* a dead member rather than failing on it:
/// membership churns as providers are replaced, and a call that landed on a
/// departed instance while a live sibling was ready would be an avoidable
/// failure.
#[derive(Debug, Default)]
pub struct MemberSet {
    members: Vec<MailboxId>,
    cursor: usize,
}

impl MemberSet {
    #[must_use]
    pub fn sole(member: MailboxId) -> Self {
        Self { members: vec![member], cursor: 0 }
    }

    /// Add `member` if absent. Idempotent, so a provider re-running `wire`
    /// after a replacement rejoins rather than duplicating itself.
    pub fn join(&mut self, member: MailboxId) {
        if !self.members.contains(&member) {
            self.members.push(member);
        }
    }

    pub fn remove(&mut self, member: MailboxId) {
        self.members.retain(|held| *held != member);
    }

    #[must_use]
    pub fn is_sole(&self, member: MailboxId) -> bool {
        self.members == [member]
    }

    #[must_use]
    pub fn members(&self) -> &[MailboxId] {
        &self.members
    }

    /// The next live member, advancing the cursor exactly once per call so a
    /// steady stream of calls spreads across the set rather than pinning the
    /// first live one.
    pub fn select(&mut self, live: &dyn Fn(MailboxId) -> bool) -> Option<MailboxId> {
        if self.members.is_empty() {
            return None;
        }
        let start = self.cursor;
        self.cursor = self.cursor.wrapping_add(1);
        (0..self.members.len()).find_map(|offset| {
            let member = self.members[(start.wrapping_add(offset)) % self.members.len()];
            live(member).then_some(member)
        })
    }
}

/// Everything a `tools/call` needs after the catalog resolved its name.
///
/// Owned rather than borrowed: the caller holds the registry mutably to advance
/// the round-robin cursor, and a returned borrow would keep that lock alive
/// across the whole encode-and-dispatch path.
#[derive(Debug, Clone)]
pub struct ToolDispatch {
    pub target: MailboxId,
    pub request_kind: KindId,
    pub request_wrapper_schema: SchemaType,
    pub output_wrapper_schema: SchemaType,
    /// Whether the tool's declared input is `Unit`, which decides how an absent
    /// `arguments` member is wrapped.
    pub unit_input: bool,
}

/// Why a named tool cannot be dispatched right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolUnavailable {
    /// No descriptor by that name — a protocol error, since the caller named
    /// something the catalog never advertised.
    Unknown,
    /// The descriptor exists but every holder has departed. Past the protocol
    /// line: the name resolved, so this is a tool execution failure.
    NoLiveTarget,
}

/// One admitted tool descriptor and its holders.
struct ToolEntry {
    descriptor: ToolDescriptor,
    request_kind: KindId,
    request_wrapper_schema: SchemaType,
    output_wrapper_schema: SchemaType,
    unit_input: bool,
    shared: bool,
    /// The equality key for a shared join: the canonical schema carriers plus
    /// the metadata a client reads. Compared byte-for-byte, because two members
    /// of one name must be indistinguishable to a caller — a set whose members
    /// disagreed about their own contract would answer differently depending on
    /// which one the cursor happened to pick.
    identity: DescriptorIdentity,
    members: MemberSet,
}

/// The byte-comparable identity of a descriptor.
#[derive(PartialEq, Eq)]
struct DescriptorIdentity {
    title: Option<String>,
    description: String,
    annotations: ToolAnnotations,
    request_kind_name: String,
    request_kind: KindId,
    request_wrapper_schema_bytes: Vec<u8>,
    output_wrapper_schema_bytes: Vec<u8>,
    output_schema_bytes: Vec<u8>,
    shared: bool,
}

/// The tool catalog.
///
/// Keyed by a `BTreeMap` so iteration is already name-sorted: the protocol's
/// listing order is lexical, and deriving it from the container rather than
/// sorting at render time means a randomized registration order cannot produce
/// a different catalog.
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolEntry>,
    frozen: bool,
    listing: Option<Value>,
    limits: RegistryLimits,
}

impl ToolRegistry {
    #[must_use]
    pub fn new(limits: RegistryLimits) -> Self {
        Self { tools: BTreeMap::new(), frozen: false, listing: None, limits }
    }

    /// Admit one `RegisterToolSelf` from the host-stamped `registrant`.
    ///
    /// `accepts` is the registrant's capability-registry verdict on the hidden
    /// request kind. It is asked *before* the claim commits, because a
    /// descriptor pointing at a kind its holder does not handle would advertise
    /// a tool whose every call warn-drops — a failure that shows up only when a
    /// client tries it, long after the registration that caused it.
    pub fn register(
        &mut self,
        registrant: MailboxId,
        payload: &RegisterToolSelf,
        accepts: &dyn Fn(MailboxId, KindId) -> bool,
    ) -> RegisterToolResult {
        match self.admit(registrant, payload, accepts) {
            Ok(()) => RegisterToolResult::Ok,
            Err(error) => RegisterToolResult::Err { error },
        }
    }

    /// The whole admission decision, as a `Result` so every refusal is one `?`
    /// and the commit is unmistakably the last statement.
    fn admit(
        &mut self,
        registrant: MailboxId,
        payload: &RegisterToolSelf,
        accepts: &dyn Fn(MailboxId, KindId) -> bool,
    ) -> Result<(), String> {
        validate_tool_name(&payload.name)?;
        validate_metadata(payload)?;

        let carriers = self.decode_carriers(payload)?;
        let recomputed = KindId(kind_id_from_parts(&payload.request_kind_name, &carriers.request_wrapper));
        if recomputed != payload.request_kind {
            return Err(format!(
                "tool `{}` declares request kind {:?}, but `{}` over its request-wrapper schema \
                 recomputes to {recomputed:?}; the descriptor does not describe the kind it points at",
                payload.name, payload.request_kind, payload.request_kind_name,
            ));
        }
        if !accepts(registrant, payload.request_kind) {
            return Err(format!(
                "tool `{}` points at request kind {:?}, which the registrant does not handle",
                payload.name, payload.request_kind,
            ));
        }

        let identity = descriptor_identity(payload);
        if let Some(existing) = self.tools.get_mut(&payload.name) {
            return join_existing(&payload.name, existing, registrant, &identity);
        }

        // A new name past the freeze would grow a catalog a client already
        // cached and cannot be told about — the server advertises
        // `listChanged: false` and has no channel to correct it.
        if self.frozen {
            return Err(format!(
                "tool `{}` is a new name after the catalog froze on the first `tools/list`; \
                 a client has already cached the advertised set",
                payload.name,
            ));
        }
        if self.tools.len() >= self.limits.maximum_registered_tools {
            return Err(format!(
                "the catalog already holds its ceiling of {} tool descriptors",
                self.limits.maximum_registered_tools,
            ));
        }

        let descriptor = self.translate_descriptor(payload, &carriers)?;
        self.check_listing_fits(&descriptor)?;

        self.tools.insert(
            payload.name.clone(),
            ToolEntry {
                descriptor,
                request_kind: payload.request_kind,
                unit_input: matches!(carriers.input, SchemaType::Unit),
                request_wrapper_schema: carriers.request_wrapper,
                output_wrapper_schema: carriers.output_wrapper,
                shared: payload.shared,
                identity,
                members: MemberSet::sole(registrant),
            },
        );
        Ok(())
    }

    /// Decode the three schema carriers and check the generated wrapper shapes.
    ///
    /// The shapes are checked rather than trusted because `SchemaType` is public
    /// and serializable: the carriers normally come from a macro that cannot
    /// produce a wrong one, but nothing at this boundary can prove that, and a
    /// hand-authored trio would otherwise put an unexecutable descriptor in the
    /// catalog.
    fn decode_carriers(&self, payload: &RegisterToolSelf) -> Result<Carriers, String> {
        let request_wrapper = self.decode_carrier(&payload.request_wrapper_schema_bytes, "request wrapper")?;
        let output_wrapper = self.decode_carrier(&payload.output_wrapper_schema_bytes, "output wrapper")?;
        let boundary = self.decode_carrier(&payload.output_schema_bytes, "boundary output")?;

        let input = single_field(&request_wrapper, "input", "request wrapper")?.clone();
        single_field(&output_wrapper, "output", "output wrapper")?;
        check_boundary(&boundary, &output_wrapper)?;

        Ok(Carriers { request_wrapper, output_wrapper, boundary, input })
    }

    fn decode_carrier(&self, bytes: &[u8], role: &str) -> Result<SchemaType, String> {
        if bytes.len() > self.limits.maximum_schema_bytes {
            return Err(format!(
                "the {role} schema carrier is {} bytes, past the {}-byte ceiling",
                bytes.len(),
                self.limits.maximum_schema_bytes,
            ));
        }
        wire::from_bytes::<SchemaType>(bytes)
            .map_err(|error| format!("the {role} schema carrier does not decode: {error}"))
    }

    /// Translate the admitted schemas into the descriptor `tools/list` renders.
    fn translate_descriptor(&self, payload: &RegisterToolSelf, carriers: &Carriers) -> Result<ToolDescriptor, String> {
        Ok(ToolDescriptor {
            name: payload.name.clone(),
            title: payload.title.clone(),
            description: payload.description.clone(),
            input_schema: translate_tool_schema(&carriers.input, self.limits.schema_budget)
                .map_err(|error| format!("the tool's input schema is not admissible: {error}"))?,
            output_schema: translate_tool_schema(&carriers.boundary, self.limits.schema_budget)
                .map_err(|error| format!("the tool's boundary output schema is not admissible: {error}"))?,
            annotations: payload.annotations,
        })
    }

    /// Refuse a descriptor that would push the rendered listing past the HTTP
    /// response ceiling.
    ///
    /// Checked here, before the insert, so a rejected registration leaves the
    /// catalog exactly as it was. Deferring it to render time would mean the
    /// first `tools/list` — a request that named nothing wrong — is the one that
    /// fails, and it would fail for every client from then on.
    fn check_listing_fits(&self, candidate: &ToolDescriptor) -> Result<(), String> {
        let mut descriptors: Vec<ToolDescriptor> = self.tools.values().map(|entry| entry.descriptor.clone()).collect();
        descriptors.push(candidate.clone());

        let rendered = list_tools_result(&descriptors).to_string().len();
        if rendered > self.limits.maximum_http_response_bytes {
            return Err(format!(
                "admitting tool `{}` would render a {rendered}-byte `tools/list`, past the {}-byte response ceiling",
                candidate.name, self.limits.maximum_http_response_bytes,
            ));
        }
        Ok(())
    }

    /// Release every membership held by `mailbox`.
    ///
    /// Descriptors survive. A name that vanished when its holder departed would
    /// shrink a catalog a client cached, and an actor replacement — the ordinary
    /// case — would look to that client like the tool being withdrawn. The
    /// descriptor stays and its calls answer `isError: true` until a holder
    /// returns.
    pub fn purge(&mut self, mailbox: MailboxId) {
        for entry in self.tools.values_mut() {
            entry.members.remove(mailbox);
        }
    }

    /// Render the catalog and freeze its names.
    ///
    /// The rendered value is cached, so every later `tools/list` is the same
    /// bytes rather than a fresh sort that could disagree with the first.
    pub fn freeze_and_list(&mut self) -> Value {
        if let Some(listing) = &self.listing {
            return listing.clone();
        }
        let descriptors: Vec<ToolDescriptor> = self.tools.values().map(|entry| entry.descriptor.clone()).collect();
        let listing = list_tools_result(&descriptors);
        self.frozen = true;
        self.listing = Some(listing.clone());
        listing
    }

    /// Resolve a name to one live holder, advancing its cursor.
    pub fn dispatch(&mut self, name: &str, live: &dyn Fn(MailboxId) -> bool) -> Result<ToolDispatch, ToolUnavailable> {
        let entry = self.tools.get_mut(name).ok_or(ToolUnavailable::Unknown)?;
        let target = entry.members.select(live).ok_or(ToolUnavailable::NoLiveTarget)?;

        Ok(ToolDispatch {
            target,
            request_kind: entry.request_kind,
            request_wrapper_schema: entry.request_wrapper_schema.clone(),
            output_wrapper_schema: entry.output_wrapper_schema.clone(),
            unit_input: entry.unit_input,
        })
    }

    /// Whether the catalog has been rendered and its names closed.
    #[must_use]
    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Admitted descriptor count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Holders of one name, in registration order. Test-facing: round-robin
    /// order is a contract, and asserting it needs the set that produced it.
    #[must_use]
    pub fn members(&self, name: &str) -> Option<&[MailboxId]> {
        self.tools.get(name).map(|entry| entry.members.members())
    }
}

/// The decoded carriers of one registration.
struct Carriers {
    request_wrapper: SchemaType,
    output_wrapper: SchemaType,
    boundary: SchemaType,
    /// The request wrapper's `input` child — the schema `tools/list` advertises
    /// and `encode_schema` validates a call's arguments against.
    input: SchemaType,
}

/// Join or re-claim a name that is already held.
fn join_existing(
    name: &str,
    existing: &mut ToolEntry,
    registrant: MailboxId,
    identity: &DescriptorIdentity,
) -> Result<(), String> {
    if !existing.shared || !identity.shared {
        // The sole exclusive holder re-registering itself is the ordinary
        // `wire`-after-replacement path, and its mailbox id is stable, so it is
        // an idempotent success rather than a conflict with itself.
        if existing.members.is_sole(registrant) && existing.identity == *identity {
            return Ok(());
        }
        return Err(format!(
            "tool `{name}` is already claimed by mailbox {:?}{}",
            existing.members.members().first(),
            if existing.shared == identity.shared {
                ""
            } else {
                "; spreading a name across actors is a joint opt-in, so an exclusive and a shared \
                 registration cannot share it"
            },
        ));
    }
    if existing.identity != *identity {
        return Err(format!(
            "tool `{name}` is a shared member set whose descriptor differs from this registration's; \
             every member must advertise byte-identical metadata and schemas"
        ));
    }
    existing.members.join(registrant);
    Ok(())
}

/// The equality key a shared join is checked against.
fn descriptor_identity(payload: &RegisterToolSelf) -> DescriptorIdentity {
    DescriptorIdentity {
        title: payload.title.clone(),
        description: payload.description.clone(),
        annotations: payload.annotations,
        request_kind_name: payload.request_kind_name.clone(),
        request_kind: payload.request_kind,
        request_wrapper_schema_bytes: payload.request_wrapper_schema_bytes.clone(),
        output_wrapper_schema_bytes: payload.output_wrapper_schema_bytes.clone(),
        output_schema_bytes: payload.output_schema_bytes.clone(),
        shared: payload.shared,
    }
}

/// `^[a-z][a-z0-9_]{0,63}$`.
///
/// Spelled out rather than matched with a regular expression because the same
/// grammar is what lets the macro paste the name verbatim into a kind name; a
/// character outside it would produce a kind name nothing can address.
fn validate_tool_name(name: &str) -> Result<(), String> {
    let refuse = || {
        Err(format!(
            "tool name `{name}` is not the accepted grammar: a lowercase letter followed by up to \
             {} more lowercase letters, digits, or underscores",
            TOOL_NAME_MAXIMUM_BYTES - 1,
        ))
    };

    if name.len() > TOOL_NAME_MAXIMUM_BYTES {
        return refuse();
    }
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return refuse(),
    }
    if characters.any(|character| !(character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_')) {
        return refuse();
    }
    Ok(())
}

fn validate_metadata(payload: &RegisterToolSelf) -> Result<(), String> {
    if payload.description.is_empty() || payload.description.len() > TOOL_DESCRIPTION_MAXIMUM_BYTES {
        return Err(format!(
            "tool `{}` must carry a description of 1 through {TOOL_DESCRIPTION_MAXIMUM_BYTES} bytes",
            payload.name,
        ));
    }
    match &payload.title {
        Some(title) if title.is_empty() || title.len() > TOOL_TITLE_MAXIMUM_BYTES => {
            Err(format!("tool `{}`'s title must be 1 through {TOOL_TITLE_MAXIMUM_BYTES} bytes", payload.name))
        }
        _ => Ok(()),
    }
}

/// Require a one-field struct with the expected field name, and hand back the
/// child schema.
fn single_field<'a>(schema: &'a SchemaType, expected: &str, role: &str) -> Result<&'a SchemaType, String> {
    match schema {
        SchemaType::Struct { fields, .. } if fields.len() == 1 && fields[0].name == expected => Ok(&fields[0].ty),
        _ => Err(format!("the {role} schema must be a struct with exactly one `{expected}` field")),
    }
}

/// Require the boundary struct's exact two-field shape.
///
/// The pairing is the load-bearing part: `inline` has to be an option over
/// *this* registration's output wrapper, or a spill and an inline result would
/// be describing different types under one advertised `outputSchema`.
fn check_boundary(boundary: &SchemaType, output_wrapper: &SchemaType) -> Result<(), String> {
    let SchemaType::Struct { fields, .. } = boundary else {
        return Err("the boundary output schema must be a struct".to_string());
    };
    if fields.len() != 2 || fields[0].name != "inline" || fields[1].name != "addressed" {
        return Err("the boundary output schema must have exactly the fields `inline` and `addressed`".to_string());
    }

    let SchemaType::Option(inline) = &fields[0].ty else {
        return Err("the boundary output schema's `inline` field must be an option".to_string());
    };
    if &**inline != output_wrapper {
        return Err("the boundary output schema's `inline` field must be an option over this registration's \
             output wrapper"
            .to_string());
    }

    let SchemaType::Option(addressed) = &fields[1].ty else {
        return Err("the boundary output schema's `addressed` field must be an option".to_string());
    };
    if **addressed != <AddressedOutput as Schema>::SCHEMA {
        return Err(
            "the boundary output schema's `addressed` field must be an option over `AddressedOutput`".to_string()
        );
    }
    Ok(())
}

/// One provider's exclusive prefix claim.
struct ProviderEntry {
    prefix: String,
    mailbox: MailboxId,
    descriptors: Vec<ResourceDescriptor>,
}

/// The resource-provider table.
///
/// Prefix claims are exclusive and longest-prefix matching wins, so two
/// providers may nest without ambiguity. Nothing here is dynamic: an address is
/// parsed and normalized before it is matched, so a provider cannot be reached
/// through a spelling it did not claim.
pub struct ResourceRegistry {
    providers: Vec<ProviderEntry>,
    frozen: bool,
    listing: Option<Value>,
    limits: RegistryLimits,
}

impl ResourceRegistry {
    #[must_use]
    pub fn new(limits: RegistryLimits) -> Self {
        Self { providers: Vec::new(), frozen: false, listing: None, limits }
    }

    pub fn register(
        &mut self,
        registrant: MailboxId,
        payload: &RegisterResourceProviderSelf,
    ) -> RegisterResourceProviderResult {
        match self.admit(registrant, payload) {
            Ok(()) => RegisterResourceProviderResult::Ok,
            Err(error) => RegisterResourceProviderResult::Err { error },
        }
    }

    fn admit(&mut self, registrant: MailboxId, payload: &RegisterResourceProviderSelf) -> Result<(), String> {
        let prefix = normalize_provider_prefix(&payload.prefix)
            .map_err(|error| format!("resource prefix `{}` is not accepted: {error}", payload.prefix))?;

        // The response store's own prefix is not claimable: an address under it
        // is minted by this capability from an unpredictable nonce, and a
        // provider holding the prefix could answer for addresses it never
        // issued.
        if prefix.starts_with(RESPONSE_RESOURCE_PREFIX) || RESPONSE_RESOURCE_PREFIX.starts_with(&prefix) {
            return Err(format!("`{RESPONSE_RESOURCE_PREFIX}` is reserved to the server's own response store"));
        }

        if let Some(existing) = self.providers.iter_mut().find(|entry| entry.prefix == prefix) {
            if existing.mailbox != registrant {
                return Err(format!("resource prefix `{prefix}` is already claimed by mailbox {:?}", existing.mailbox));
            }
            existing.descriptors.clone_from(&payload.descriptors);
            return Ok(());
        }

        // After the discoverable catalog froze, a *new* prefix may still be
        // claimed as long as it lists nothing: concrete addresses under a
        // provider prefix are dynamic by design (that is how content hashes
        // work), and only the listed set is what a client cached.
        if self.frozen && !payload.descriptors.is_empty() {
            return Err(format!(
                "resource prefix `{prefix}` carries discoverable descriptors, and the discoverable \
                 catalog froze on the first `resources/list`",
            ));
        }

        let admitted: usize = self.providers.iter().map(|entry| entry.descriptors.len()).sum();
        if admitted + payload.descriptors.len() > self.limits.maximum_discoverable_resources {
            return Err(format!(
                "admitting `{prefix}` would pass the ceiling of {} discoverable resource descriptors",
                self.limits.maximum_discoverable_resources,
            ));
        }

        let mut rendered: Vec<ResourceDescriptor> =
            self.providers.iter().flat_map(|entry| entry.descriptors.clone()).collect();
        rendered.extend(payload.descriptors.iter().cloned());
        let bytes = list_resources_result(&rendered).to_string().len();
        if bytes > self.limits.maximum_http_response_bytes {
            return Err(format!(
                "admitting `{prefix}` would render a {bytes}-byte `resources/list`, past the {}-byte \
                 response ceiling",
                self.limits.maximum_http_response_bytes,
            ));
        }

        self.providers.push(ProviderEntry { prefix, mailbox: registrant, descriptors: payload.descriptors.clone() });
        Ok(())
    }

    /// The provider holding the longest prefix matching `uri`.
    #[must_use]
    pub fn resolve(&self, uri: &str) -> Option<MailboxId> {
        self.providers
            .iter()
            .filter(|entry| uri.starts_with(&entry.prefix))
            .max_by_key(|entry| entry.prefix.len())
            .map(|entry| entry.mailbox)
    }

    pub fn purge(&mut self, mailbox: MailboxId) {
        self.providers.retain(|entry| entry.mailbox != mailbox);
    }

    pub fn freeze_and_list(&mut self) -> Value {
        if let Some(listing) = &self.listing {
            return listing.clone();
        }
        let descriptors: Vec<ResourceDescriptor> =
            self.providers.iter().flat_map(|entry| entry.descriptors.clone()).collect();
        let listing = list_resources_result(&descriptors);
        self.frozen = true;
        self.listing = Some(listing.clone());
        listing
    }

    #[must_use]
    pub fn is_frozen(&self) -> bool {
        self.frozen
    }
}
