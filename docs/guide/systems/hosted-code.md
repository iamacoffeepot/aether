# Hosted code and live replacement

Hosted code runs inside a substrate but remains actor-tier code. The host owns
isolation and scheduling; the guest owns application state and mail behavior.

| Surface | Chapter | Use it for |
|---|---|---|
| Wasm actor modules | [Components and lifecycle](components.md) | first-class mailboxes, state, replies, replacement |
| In-cluster filters | [Behaviors](behaviors.md) | small fail-open transforms at a tree position |
| Names/contracts/transforms | [Inventory and transforms](inventory-and-transforms.md) | engine-specific discovery plus the MCP build's bounded transform inventory |

Component bytes in the hub registry are artifacts. A loaded component is an
engine-local instance. A behavior script is a second wasm artifact class hosted
inside a component export. Keep these identities separate when building,
uploading, selecting, loading, and replacing.

Read [Guest and native boundaries](../architecture/guest-native-boundary.md)
before changing exports, schema, feature tiers, or state migration.
