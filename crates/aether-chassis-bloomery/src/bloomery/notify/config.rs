//! The notification reactor's ADR-0090 derive-`Config` member (#5166).

/// Where the operator webhook URL is read from.
///
/// A **file path**, never the URL: the URL is a credential, and anything that
/// rides argv or the environment is visible in a process listing, an
/// `/proc/*/environ` read, a crash dump, and every `--describe` of the binary.
/// The path is the harmless half — it names a host-local file whose mode the
/// operator controls — and the reactor reads the URL out of it once at boot.
///
/// Unset resolves to `None`, which mounts the reactor disabled with one boot
/// log line, exactly as an unconfigured token mounts the mirror disabled. A
/// coordinator that has not been given somewhere to shout is not a broken
/// coordinator.
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER_BLOOMERY_NOTIFY", cli_prefix = "notify")]
pub struct NotifyConfig {
    /// Absolute or working-directory-relative path to a file whose contents
    /// are the webhook URL (surrounding whitespace trimmed); unset → the
    /// reactor mounts disabled.
    #[config(env = "AETHER_BLOOMERY_NOTIFY_WEBHOOK_FILE")]
    pub webhook_file: Option<String>,
}
