//! Per-window render targets and the map that keys them by [`WindowId`]
//! (ADR-0161).
//!
//! Desktop-only: a surfaceless runtime has no window to attach, so the whole
//! module sits behind the parent's `#[cfg(feature = "desktop")]` gate on
//! `mod target`.
//!
//! Two things live here, and the split matters. [`WindowTargets`] is generic
//! over the target type and knows only about *identity* — attach refuses a
//! duplicate id, detach hands the target back, and capture selection resolves
//! an id to a target or explains why it cannot. [`RenderTarget`] is the
//! concrete per-window state: the retained window handle, its swapchain, and
//! the occlusion flag.
//!
//! The runtime keeps the state-field bookkeeping (which GPU is booted, whether
//! the runtime is surfaceless) and calls the two constructors below for the
//! parts that touch wgpu.

use std::collections::BTreeMap;
use std::sync::Arc;

use aether_kinds::WindowId;
use winit::window::Window;

use super::pipeline::RenderGpu;
use super::surface::{acquire_surface_texture, attach_surface, boot_surface, build_wireframe_overlay_pipeline};

/// Window-keyed render targets. Generic over the target so the identity rules
/// — no duplicate attach, detach returns the target, capture selection
/// resolves or explains — are stated once and read without wgpu in the way.
pub struct WindowTargets<T> {
    entries: BTreeMap<WindowId, T>,
}

impl<T> Default for WindowTargets<T> {
    fn default() -> Self {
        Self { entries: BTreeMap::new() }
    }
}

impl<T> WindowTargets<T> {
    /// Insert a target for `id`, building it only after the duplicate check
    /// passes. `build` returns the target plus a caller-chosen extra, so a
    /// first attachment can hand back the GPU it had to boot along the way.
    pub fn attach_with<R>(
        &mut self,
        id: WindowId,
        build: impl FnOnce() -> Result<(T, R), String>,
    ) -> Result<R, String> {
        if self.entries.contains_key(&id) {
            return Err(format!("render target for window {} is already attached", id.0));
        }
        let (target, result) = build()?;
        self.entries.insert(id, target);
        Ok(result)
    }

    pub fn detach(&mut self, id: WindowId) -> Option<T> {
        self.entries.remove(&id)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Build a complete replacement map without mutating the live targets.
    /// The lowest [`WindowId`] is always passed to `build_first`; every later
    /// target sees the caller-chosen context that first build returned. A
    /// failure at any point drops only the staged values.
    fn stage_replacement_with<U, C>(
        &self,
        build_first: impl FnOnce(WindowId, &T) -> Result<(U, C), String>,
        mut build_later: impl FnMut(WindowId, &T, &C) -> Result<U, String>,
    ) -> Result<(WindowTargets<U>, C), String> {
        let mut live = self.entries.iter();
        let Some((&first_id, first_target)) = live.next() else {
            return Err("desktop device replacement requires at least one retained window target".to_owned());
        };
        let (first_target, context) = build_first(first_id, first_target)?;
        let mut staged = BTreeMap::new();
        staged.insert(first_id, first_target);

        for (&id, target) in live {
            staged.insert(id, build_later(id, target, &context)?);
        }

        Ok((WindowTargets { entries: staged }, context))
    }

    /// The attached target for `id`, if there is one. `None` is the ordinary
    /// case for a window in a fan-out list that has since detached, so callers
    /// skip it rather than treating it as an error.
    pub fn get_mut(&mut self, id: WindowId) -> Option<&mut T> {
        self.entries.get_mut(&id)
    }

    pub fn set_occluded(&mut self, id: WindowId, occluded: bool, update: impl FnOnce(&mut T, bool)) -> bool {
        let Some(target) = self.entries.get_mut(&id) else {
            return false;
        };
        update(target, occluded);
        true
    }

    /// Resolve a capture's window selection. `Ok(true)` means the named target
    /// is attached and visible; `Ok(false)` means no targets exist at all, so
    /// the caller may fall through to its surfaceless path.
    pub fn validate_capture_selection(
        &self,
        window: Option<WindowId>,
        is_occluded: impl Fn(&T) -> bool,
    ) -> Result<bool, String> {
        let Some(id) = window else {
            if self.entries.is_empty() {
                return Ok(false);
            }
            return Err(
                "capture_frame failed: a window target is required when desktop targets are attached".to_owned()
            );
        };
        let target =
            self.entries.get(&id).ok_or_else(|| format!("capture_frame failed: unknown window target {}", id.0))?;
        if is_occluded(target) {
            return Err(format!("capture_frame failed: window target {} is occluded", id.0));
        }
        Ok(true)
    }
}

pub struct RenderTarget {
    /// Retained with its surface so detachment drops render's native-window
    /// ownership before the window manager completes close.
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pub occluded: bool,
}

/// The GPU handles a first window attachment has to boot before any target
/// exists, returned so the runtime installs them on itself.
pub struct FirstWindowGpu {
    pub context: DesktopGpuContext,
    pub gpu: RenderGpu,
    pub wire_pipeline: Option<wgpu::RenderPipeline>,
}

/// Instance + adapter, retained past boot so a later attachment can create its
/// surface against the same device the first window selected.
pub struct DesktopGpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
}

impl RenderTarget {
    /// Rebuild every retained surface against one replacement device. The
    /// canonical first window selects the adapter, device, and format; later
    /// windows must attach to that exact context. The live map remains intact
    /// unless the caller publishes the returned map.
    pub fn build_replacement_targets(
        targets: &WindowTargets<Self>,
        wireframe: Option<&str>,
        vertex_buffer_bytes: usize,
    ) -> Result<(WindowTargets<Self>, FirstWindowGpu), String> {
        targets.stage_replacement_with(
            |_id, live| {
                let size = live.window.inner_size();
                let (mut target, first_gpu) = Self::boot_first(
                    Arc::clone(&live.window),
                    (size.width, size.height),
                    wireframe,
                    vertex_buffer_bytes,
                )?;
                target.occluded = live.occluded;
                Ok((target, first_gpu.expect("boot_first always returns the selected desktop GPU")))
            },
            |_id, live, first_gpu| {
                let size = live.window.inner_size();
                let (mut target, install) = Self::attach_to_booted_gpu(
                    &first_gpu.context,
                    &first_gpu.gpu.device,
                    Arc::clone(&live.window),
                    (size.width, size.height),
                    first_gpu.gpu.color_format,
                )?;
                debug_assert!(install.is_none(), "later windows never replace the shared desktop GPU");
                target.occluded = live.occluded;
                Ok(target)
            },
        )
    }

    /// Attach a window to the device an earlier attachment already selected.
    /// Fails if the window's surface cannot offer `format`, which is what
    /// keeps every target copy-compatible with the shared color texture.
    pub fn attach_to_booted_gpu(
        context: &DesktopGpuContext,
        device: &wgpu::Device,
        window: Arc<Window>,
        size: (u32, u32),
        format: wgpu::TextureFormat,
    ) -> Result<(Self, Option<FirstWindowGpu>), String> {
        let attached = attach_surface(&context.instance, &context.adapter, device, Arc::clone(&window), size, format)?;
        Ok((Self { window, surface: attached.surface, config: attached.config, occluded: false }, None))
    }

    /// Attach the first window, booting the adapter, device, and shared
    /// pipelines along the way and handing them back for the runtime to keep.
    pub fn boot_first(
        window: Arc<Window>,
        size: (u32, u32),
        wireframe: Option<&str>,
        vertex_buffer_bytes: usize,
    ) -> Result<(Self, Option<FirstWindowGpu>), String> {
        let booted = boot_surface(Arc::clone(&window), size, wireframe)?;
        let gpu = RenderGpu::new(
            Arc::clone(&booted.device),
            Arc::clone(&booted.queue),
            booted.format,
            booted.config.width,
            booted.config.height,
            booted.polygon_mode,
            vertex_buffer_bytes,
        );
        let wire_pipeline = booted
            .build_overlay
            .then(|| build_wireframe_overlay_pipeline(&booted.device, gpu.color_format, &gpu.pipeline.pipeline_layout));

        Ok((
            Self { window, surface: booted.surface, config: booted.config, occluded: false },
            Some(FirstWindowGpu {
                context: DesktopGpuContext { instance: booted.instance, adapter: booted.adapter },
                gpu,
                wire_pipeline,
            }),
        ))
    }

    /// Reconfigure to the window's current size and acquire a swapchain image.
    /// `None` when the target is occluded or degenerate, which is the signal
    /// to skip it for this frame rather than an error.
    pub fn prepare_frame(&mut self, device: &wgpu::Device) -> Option<(u32, u32, Option<wgpu::SurfaceTexture>)> {
        if self.occluded {
            return None;
        }
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return None;
        }
        if size.width != self.config.width || size.height != self.config.height {
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(device, &self.config);
        }
        let surface_texture = acquire_surface_texture(&self.surface, device, &self.config);
        Some((self.config.width, self.config.height, surface_texture))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct TestTarget {
        occluded: bool,
    }

    /// Target bookkeeping is transactional: failed and duplicate attachments
    /// do not replace entries; occlusion and removal stay local to one id.
    ///
    /// Instantiated over `bool` rather than `RenderTarget` — every rule under
    /// test is about identity and ordering, so a target type that needs no GPU
    /// keeps the test running on a driverless box.
    #[test]
    fn window_target_bookkeeping_is_transactional_and_target_local() {
        let mut targets = WindowTargets::<bool>::default();
        let failed: Result<(), String> = targets.attach_with(WindowId(1), || Err("surface failed".to_owned()));
        assert!(failed.is_err());
        assert!(targets.entries.is_empty(), "a failed builder must not insert a target");

        targets.attach_with(WindowId(1), || Ok((false, ()))).expect("first target attaches");
        let duplicate: Result<(), String> =
            targets.attach_with(WindowId(1), || panic!("duplicate validation must run before the target builder"));
        assert!(duplicate.expect_err("duplicate id is rejected").contains("already attached"));
        targets.attach_with(WindowId(2), || Ok((false, ()))).expect("second target attaches");

        assert!(targets.set_occluded(WindowId(1), true, |target, value| *target = value));
        assert!(targets.entries[&WindowId(1)]);
        assert!(!targets.entries[&WindowId(2)]);
        assert!(targets.validate_capture_selection(Some(WindowId(1)), |target| *target).is_err());
        assert_eq!(targets.validate_capture_selection(Some(WindowId(2)), |target| *target), Ok(true));
        assert!(targets.validate_capture_selection(Some(WindowId(99)), |target| *target).is_err());
        assert!(targets.validate_capture_selection(None, |target| *target).is_err());

        assert_eq!(targets.detach(WindowId(1)), Some(true));
        assert!(targets.entries.contains_key(&WindowId(2)), "detaching one target leaves the other live");
    }

    #[test]
    fn staged_replacement_uses_canonical_first_id_and_preserves_target_state() {
        let mut targets = WindowTargets::default();
        for (id, occluded) in [(8, true), (2, false), (5, true)] {
            targets.attach_with(WindowId(id), || Ok((TestTarget { occluded }, ()))).expect("test target attaches");
        }
        let build_order = RefCell::new(Vec::new());
        let (staged, first_id) = targets
            .stage_replacement_with(
                |id, live| {
                    build_order.borrow_mut().push(id);
                    Ok((live.clone(), id))
                },
                |id, live, selected| {
                    assert_eq!(*selected, WindowId(2), "later targets share the canonical first context");
                    build_order.borrow_mut().push(id);
                    Ok(live.clone())
                },
            )
            .expect("complete replacement stages");

        assert_eq!(first_id, WindowId(2));
        assert_eq!(build_order.into_inner(), [WindowId(2), WindowId(5), WindowId(8)]);
        assert_eq!(staged.entries, targets.entries, "keys and occlusion flags survive replacement");
    }

    #[test]
    fn later_staging_failure_leaves_live_target_map_unchanged() {
        let mut targets = WindowTargets::default();
        for (id, occluded) in [(2, false), (5, true), (8, false)] {
            targets.attach_with(WindowId(id), || Ok((TestTarget { occluded }, ()))).expect("test target attaches");
        }
        let before = targets.entries.clone();

        let result = targets.stage_replacement_with(
            |_id, live| Ok((live.clone(), ())),
            |id, live, _context| {
                if id == WindowId(8) {
                    Err("later surface failed".to_owned())
                } else {
                    Ok(live.clone())
                }
            },
        );

        let Err(error) = result else {
            panic!("later failure must reject the staged map");
        };
        assert_eq!(error, "later surface failed");
        assert_eq!(targets.entries, before, "the live map is not mutated during staging");
    }
}
