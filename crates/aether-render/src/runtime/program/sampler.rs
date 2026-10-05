//! The samplers a program's inputs bind (ADR-0246 decision 5): one per
//! combination of the bound texture's filter and the binding's declared
//! wrap and mips, built once per device.
//!
//! The substrate's shared pair (`TextureBindings::sampler` /
//! `nearest_sampler`) clamps and reads no mip chain, which is all the
//! quad and material paths ask for. A program binding declares its own
//! address mode and mip filter, so the combinations live here, with the
//! cap that owns the declaration.

use crate::{Mips, Wrap};

/// Linear or nearest: the half of a sampler the bound texture decides.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) enum Filter {
    Linear,
    Nearest,
}

const FILTERS: [Filter; 2] = [Filter::Linear, Filter::Nearest];
const WRAPS: [Wrap; 2] = [Wrap::Clamp, Wrap::Repeat];
const MIPS: [Mips; 2] = [Mips::Base, Mips::Chain];

/// Every sampler a program input can ask for, held in build order. A `Nearest` sampler is non-filtering in
/// all three of its filters, so it binds under a filtering layout entry
/// and a non-filtering one alike.
pub(super) struct ProgramSamplers {
    samplers: Vec<wgpu::Sampler>,
}

impl ProgramSamplers {
    pub(super) fn build(device: &wgpu::Device) -> Self {
        let mut samplers = Vec::with_capacity(FILTERS.len() * WRAPS.len() * MIPS.len());
        for filter in FILTERS {
            for wrap in WRAPS {
                for mips in MIPS {
                    samplers.push(build_sampler(device, filter, wrap, mips));
                }
            }
        }
        Self { samplers }
    }

    /// The sampler for a texture filtered `filter` at a binding that
    /// declared `wrap` and `mips`.
    pub(super) fn get(&self, filter: Filter, wrap: Wrap, mips: Mips) -> &wgpu::Sampler {
        &self.samplers[Self::index(filter, wrap, mips)]
    }

    /// Position in build order: filter outermost, mips innermost.
    fn index(filter: Filter, wrap: Wrap, mips: Mips) -> usize {
        let filter = match filter {
            Filter::Linear => 0,
            Filter::Nearest => 1,
        };
        let wrap = match wrap {
            Wrap::Clamp => 0,
            Wrap::Repeat => 1,
        };
        let mips = match mips {
            Mips::Base => 0,
            Mips::Chain => 1,
        };
        (filter * WRAPS.len() + wrap) * MIPS.len() + mips
    }
}

/// `Mips::Base` pins the level of detail to zero, so a texture that
/// carries a chain still reads its base level; `Mips::Chain` leaves the
/// range open and blends between levels when the texture is
/// linear-filtered.
fn build_sampler(device: &wgpu::Device, filter: Filter, wrap: Wrap, mips: Mips) -> wgpu::Sampler {
    let address_mode = match wrap {
        Wrap::Clamp => wgpu::AddressMode::ClampToEdge,
        Wrap::Repeat => wgpu::AddressMode::Repeat,
    };
    let texel_filter = match filter {
        Filter::Linear => wgpu::FilterMode::Linear,
        Filter::Nearest => wgpu::FilterMode::Nearest,
    };
    let (mipmap_filter, lod_max_clamp) = match (mips, filter) {
        (Mips::Base, _) => (wgpu::MipmapFilterMode::Nearest, 0.0),
        (Mips::Chain, Filter::Linear) => (wgpu::MipmapFilterMode::Linear, 32.0),
        (Mips::Chain, Filter::Nearest) => (wgpu::MipmapFilterMode::Nearest, 32.0),
    };
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("aether program input sampler"),
        address_mode_u: address_mode,
        address_mode_v: address_mode,
        address_mode_w: address_mode,
        mag_filter: texel_filter,
        min_filter: texel_filter,
        mipmap_filter,
        lod_min_clamp: 0.0,
        lod_max_clamp,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `get` reads the table by arithmetic while `build` fills it by
    /// nested loops, so a reordered loop or a wrong stride would hand a
    /// `Repeat` binding the clamped sampler with no error anywhere. The
    /// index of every combination must be its position in build order.
    #[test]
    fn index_matches_build_order() {
        let mut position = 0;
        for filter in FILTERS {
            for wrap in WRAPS {
                for mips in MIPS {
                    assert_eq!(ProgramSamplers::index(filter, wrap, mips), position, "{filter:?} {wrap:?} {mips:?}");
                    position += 1;
                }
            }
        }
    }
}
