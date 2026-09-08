# Authorizing an instruction bundle and enabling the bloom reader

**Class:** drive-only (nothing here rebuilds aether beyond the coordinator
binary and `xtask`). Read
[Supervising the coordinator with systemd](supervising-the-coordinator.md)
first: this recipe edits that host's environment file and restarts that unit.

Two host decisions live here, and they are separate on purpose.

The first is **which model-process instruction bundle this host authorizes**
([ADR-0214](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0214-sealed-model-process-instructions.md)).
Every model lane passes a fail-closed provenance gate before it may reach a
model: the dispatch's sealed configuration must pin a bundle, the stored bytes
must re-address to that pin, the bundle must be complete, and this host must
have authorized that exact digest. A coordinator that authorizes nothing
dispatches no model lane at all.

The second is **whether this host runs the bloom-level reader**
([ADR-0216](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0216-the-line-ends-in-a-reader-that-files-what-it-will-not-fix.md) §4).
The reader reads what a landed bloom left behind and files the work it will not
fix, as open unapproved commissions. Its seat — `claude-opus-5` at high effort,
once per landing — is a standing cost the ADR deliberately does not decide.

They are separate because the gate would otherwise fold them together. It
validates *every* instruction field before any lane dispatches, so the first
authorized bundle has to carry the reader's text whether or not you want a
standing read — and once a bundle carries it, every landing would dispatch one.
The knob below is what keeps "these instructions exist" apart from "spend that
seat on every bloom".

## 1. Assemble the bundle

```bash
cargo xtask bloom instructions
```

The command assembles a `ModelProcessInstructions` value from this
repository's own instruction sources — the `construct.implement`,
`review.critic` and `scope.fill` instruction files, the curated lane context,
and the framing texts the lanes and the coordinator compose their prompts from —
validates that no field is empty, and prints the bundle's content address:

```
aether.bloomery.model_process_instructions 9f2c…  (64 hex characters)
authorize it by naming that address in AETHER_BLOOMERY_AUTHORIZED_INSTRUCTIONS …
```

Nothing has been written or sent yet. Add `--out body.json` to keep the
`POST /configs` body for review before you record it.

## 2. Record it as configuration

```bash
cargo xtask bloom instructions --record
```

This is the ordinary `POST /configs` route — the same one a `--config` flag on a
seal goes through. It stores the bundle's bytes and hands back their address,
which the command checks against the one it derived locally.

**Recording is not authorizing.** Content in the store is content; whether it
may serve as this host's model-process policy is answered only in step 3.
Knowing a digest, uploading it, or naming it in a request authorizes nothing.

## 3. Authorize it on the host

Name the address in the coordinator's environment file
(`~/.config/bloomery/bloomery.env`, per the supervision recipe):

```
AETHER_BLOOMERY_AUTHORIZED_INSTRUCTIONS=9f2c…
```

The value is a comma-separated list, so a rotation can authorize the outgoing
and incoming bundles at once. It is seeded into the coordinator's store at
executor-reactor boot and **replaces** the previous set, so removing an address
withdraws it. Restart the unit for it to take effect.

A coordinator that boots with nothing authorized logs one `warn` saying so, and
every model dispatch refuses — visibly, as a machinery roll against the member,
not silently.

## 4. Seal blooms that pin it

A bloom carries the pin; the host carries the authorization. A bloom sealed
before this bundle existed pins nothing and cannot start another model attempt —
it needs a successor
([ADR-0214](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0214-sealed-model-process-instructions.md) §Migration).
Seal new blooms with the bundle in their configuration:

```bash
cargo xtask bloom seal --config aether.bloomery.model_process_instructions=body.json …
```

## 5. Turn the reader on, when you decide its seat

Off by default:

```
# AETHER_BLOOMERY_RETROSPECT_READER_ENABLED=false
```

With the knob off, a bloom still lands normally and its `Study` stage is
journaled as a study that did not pass, carrying the reason — so a missing study
reads as a decision rather than as a silence. Nothing wedges, no member is held,
and no order is spent.

Set it to `true` and restart the unit when you have decided the seat. From that
point every landed bloom dispatches one `retrospect.read`, and its filings
appear as open unapproved commissions on the bloom's receipt. They are inert
until a person scopes and approves them: a machine-authored work order acquires
no authority from having been emitted by an authorized process
([ADR-0216](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0216-the-line-ends-in-a-reader-that-files-what-it-will-not-fix.md) §3).

Watch two things after you turn it on:

- **The pile.** Volume is bounded by nothing but the model, and an unread pile
  is the failure mode the ADR names by name.
- **The forecast.** A standing read shifts every bloom's actual tokens and
  worker seconds, so either the forecasts budget it or every bloom grades over
  on two axes.
