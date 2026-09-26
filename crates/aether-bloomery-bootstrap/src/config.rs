//! The bootstrap's config: the two images to import and the two actors to mail.

use aether_actor::ActorInitError;
use aether_data::{ActorPath, Kind};
use aether_workspace::ImageRef;

/// What the operator hands `load_component` as `config`: the base and
/// toolchain images `publish.sh` printed, and the paths of the journal owner
/// and the bundle driver the script mails.
///
/// Each field is optional on the wire only because a component's config must
/// have a default, which an empty config boots. [`Self::into_bootstrap`], which
/// `init` runs, refuses a config missing any field, so the actor never holds
/// part of one. Each present field validates on decode.
#[aether_data::kind(name = "aether.bloomery.bootstrap.config", default, eq, no_serde)]
pub struct BootstrapConfig {
    /// The distro userland image, `base=` in `publish.sh`'s output.
    pub base: Option<ImageRef>,
    /// The Rust toolchain image, `toolchain=` in `publish.sh`'s output.
    pub toolchain: Option<ImageRef>,
    /// The journal owner, `aether.bloomery.journal:journal` on a Bloomery engine.
    pub journal: Option<ActorPath>,
    /// The bundle driver, `aether.bloomery.driver:driver` on a Bloomery engine.
    pub driver: Option<ActorPath>,
}

/// A whole config: both images and both peer paths.
#[derive(Debug, Clone)]
pub struct Bootstrap {
    /// The distro userland image.
    pub base: ImageRef,
    /// The Rust toolchain image.
    pub toolchain: ImageRef,
    /// The journal owner's path.
    pub journal: ActorPath,
    /// The bundle driver's path.
    pub driver: ActorPath,
}

impl BootstrapConfig {
    /// The whole config.
    ///
    /// # Errors
    ///
    /// An [`ActorInitError`] naming the first missing field.
    pub fn into_bootstrap(self) -> Result<Bootstrap, ActorInitError> {
        Ok(Bootstrap {
            base: required(self.base, "base")?,
            toolchain: required(self.toolchain, "toolchain")?,
            journal: required(self.journal, "journal")?,
            driver: required(self.driver, "driver")?,
        })
    }
}

/// `field`, or the refusal naming it.
fn required<T>(field: Option<T>, name: &str) -> Result<T, ActorInitError> {
    field.ok_or_else(|| ActorInitError::new(format!("{} has no `{name}`", BootstrapConfig::NAME)))
}
