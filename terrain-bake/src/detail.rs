//! Detail synthesis (design doc §5.4): upsample to `target_res_m` (bicubic),
//! then add slope-modulated FBM octaves — plains stay smooth, steep terrain
//! gets rocky high-frequency texture — while leaving water and the capital
//! footprint untouched (no un-flattening the buildable city, no texturing
//! open water).

use crate::config::DetailConfig;
use crate::grid::Grid;
use crate::stylize::{compress_mask_horizontal, resample_footprint, FootprintMask};
use crate::water::WaterMask;

/// Catmull-Rom cubic basis weights for the 4 taps around a fractional
/// offset `t` in `[0, 1]` (taps at `-1, 0, 1, 2`) — the standard "bicubic"
/// the design doc asks for.
fn cubic_weights(t: f32) -> [f32; 4] {
    let t2 = t * t;
    let t3 = t2 * t;
    [
        -0.5 * t3 + t2 - 0.5 * t,
        1.5 * t3 - 2.5 * t2 + 1.0,
        -1.5 * t3 + 2.0 * t2 + 0.5 * t,
        0.5 * t3 - 0.5 * t2,
    ]
}

fn bicubic_sample(grid: &Grid, x: f32, y: f32) -> f32 {
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let wx = cubic_weights(fx);
    let wy = cubic_weights(fy);
    let at = |gx: i32, gy: i32| -> f32 {
        let cx = gx.clamp(0, grid.width as i32 - 1) as usize;
        let cy = gy.clamp(0, grid.height as i32 - 1) as usize;
        grid.get(cx, cy)
    };
    let mut result = 0.0;
    for j in 0..4 {
        let mut row = 0.0;
        for i in 0..4 {
            row += wx[i] * at(x0 - 1 + i as i32, y0 - 1 + j as i32);
        }
        result += wy[j] * row;
    }
    result
}

/// Resample `grid` to `target_res_m` (bicubic) — a no-op if it's already at
/// that resolution (true for every config so far; #65 lands the capability,
/// finer bakes are free to use it later).
pub fn upsample_bicubic(grid: &Grid, target_res_m: f32) -> Grid {
    if (grid.cell_size_m - target_res_m).abs() < 1e-6 {
        return grid.clone();
    }
    let scale = grid.cell_size_m / target_res_m;
    let new_width = ((grid.width as f32) * scale).round().max(1.0) as usize;
    let new_height = ((grid.height as f32) * scale).round().max(1.0) as usize;
    let mut out = Grid::new(new_width, new_height, target_res_m);
    for gy in 0..new_height {
        for gx in 0..new_width {
            out.set(gx, gy, bicubic_sample(grid, gx as f32 / scale, gy as f32 / scale));
        }
    }
    out
}

/// Gradient magnitude (rise/run, not an angle) via forward differences —
/// deliberately the same shape of calculation `dump::write_hillshade_png`
/// already uses, just as a scalar instead of a shading dot-product.
fn slope_at(grid: &Grid, gx: usize, gy: usize) -> f32 {
    let h = grid.get(gx, gy);
    let hx = grid.get((gx + 1).min(grid.width - 1), gy);
    let hy = grid.get(gx, (gy + 1).min(grid.height - 1));
    let dzdx = (hx - h) / grid.cell_size_m;
    let dzdy = (hy - h) / grid.cell_size_m;
    (dzdx * dzdx + dzdy * dzdy).sqrt()
}

/// Same deterministic splitmix-style hash `synth.rs`/`rust_server::world`
/// use elsewhere in this pipeline — not `rand`, since detail texture must
/// reproduce byte-identically forever from the same seed (design doc §8).
fn hash_corner(gx: i32, gy: i32, seed: u32) -> f32 {
    let mut h = (gx as u32)
        .wrapping_mul(374_761_393)
        .wrapping_add((gy as u32).wrapping_mul(668_265_263))
        .wrapping_add(seed.wrapping_mul(2_246_822_519));
    h = (h ^ (h >> 15)).wrapping_mul(2_246_822_519);
    h = (h ^ (h >> 13)).wrapping_mul(3_266_489_917);
    h ^= h >> 16;
    (h as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// Smoothstep-interpolated value noise at unit grid cells, in `[-1, 1]`.
fn value_noise(x: f32, y: f32, seed: u32) -> f32 {
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let sx = fx * fx * (3.0 - 2.0 * fx);
    let sy = fy * fy * (3.0 - 2.0 * fy);
    let h00 = hash_corner(x0, y0, seed);
    let h10 = hash_corner(x0 + 1, y0, seed);
    let h01 = hash_corner(x0, y0 + 1, seed);
    let h11 = hash_corner(x0 + 1, y0 + 1, seed);
    let top = h00 + (h10 - h00) * sx;
    let bot = h01 + (h11 - h01) * sx;
    top + (bot - top) * sy
}

/// Fractal Brownian motion: `octaves` layers of [`value_noise`], each
/// `lacunarity` times higher frequency and `gain` times lower amplitude than
/// the last, normalized back to roughly `[-1, 1]` regardless of octave count.
fn fbm(wx: f32, wy: f32, octaves: u32, base_freq: f32, lacunarity: f32, gain: f32, seed: u32) -> f32 {
    let mut sum = 0.0;
    let mut amplitude = 1.0;
    let mut frequency = base_freq;
    let mut max_amp = 0.0;
    for octave in 0..octaves {
        sum += amplitude * value_noise(wx * frequency, wy * frequency, seed.wrapping_add(octave));
        max_amp += amplitude;
        amplitude *= gain;
        frequency *= lacunarity;
    }
    if max_amp > 0.0 { sum / max_amp } else { 0.0 }
}

/// The full stage: upsample, then add slope-modulated FBM detail — skipped
/// entirely inside `mask` (water) and `footprint` (the capital footprint),
/// both resampled to match the upsampled grid first. `octaves == 0` (the
/// config default) is a no-op past the upsample.
pub fn run_detail_stage(
    grid: &Grid,
    mask: &WaterMask,
    footprint: &FootprintMask,
    config: &DetailConfig,
    target_res_m: f32,
) -> Grid {
    let mut out = upsample_bicubic(grid, target_res_m);
    if config.octaves == 0 || config.base_amp_m <= 0.0 {
        return out;
    }
    let scale = grid.cell_size_m / target_res_m;
    let mask_up = compress_mask_horizontal(mask, scale);
    let footprint_up = resample_footprint(footprint, scale);
    let base = out.clone(); // slope/heights read from the pre-detail grid, not the one being mutated

    let base_freq = 1.0 / config.base_wavelength_m.max(1e-6);
    for gy in 0..out.height {
        for gx in 0..out.width {
            if mask_up.get(gx, gy) || footprint_up.get(gx, gy) {
                continue;
            }
            let slope = slope_at(&base, gx, gy).min(5.0);
            let amp = config.base_amp_m * slope.powf(config.slope_amp_curve);
            if amp <= 0.0 {
                continue;
            }
            let wx = gx as f32 * base.cell_size_m;
            let wy = gy as f32 * base.cell_size_m;
            let n = fbm(wx, wy, config.octaves, base_freq, config.lacunarity, config.gain, config.seed);
            out.set(gx, gy, base.get(gx, gy) + n * amp);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DetailConfig;

    fn detail_config() -> DetailConfig {
        DetailConfig {
            octaves: 4,
            base_amp_m: 3.0,
            slope_amp_curve: 1.5,
            lacunarity: 2.0,
            gain: 0.5,
            base_wavelength_m: 20.0,
            seed: 7,
        }
    }

    /// A grid with a flat plain (west half) and a steep ramp (east half),
    /// large enough for the FBM's lowest octave to actually vary.
    fn plain_and_ramp_grid() -> Grid {
        let mut g = Grid::new(40, 20, 5.0);
        for gy in 0..20 {
            for gx in 0..40 {
                let h = if gx < 20 { 10.0 } else { (gx - 20) as f32 * 8.0 };
                g.set(gx, gy, h);
            }
        }
        g
    }

    #[test]
    fn detail_is_a_no_op_at_zero_octaves() {
        let grid = plain_and_ramp_grid();
        let mask = WaterMask::new(grid.width, grid.height);
        let footprint = FootprintMask::none(grid.width, grid.height);
        let out = run_detail_stage(&grid, &mask, &footprint, &DetailConfig::default(), grid.cell_size_m);
        assert_eq!(out, grid);
    }

    #[test]
    fn steep_terrain_gets_more_texture_than_flat_plains() {
        let grid = plain_and_ramp_grid();
        let mask = WaterMask::new(grid.width, grid.height);
        let footprint = FootprintMask::none(grid.width, grid.height);
        let out = run_detail_stage(&grid, &mask, &footprint, &detail_config(), grid.cell_size_m);

        let plain_delta: f32 =
            (0..20).flat_map(|gy| (2..18).map(move |gx| (gx, gy))).map(|(gx, gy)| (out.get(gx, gy) - grid.get(gx, gy)).abs()).sum::<f32>() / (16 * 20) as f32;
        let ramp_delta: f32 = (0..20)
            .flat_map(|gy| (22..38).map(move |gx| (gx, gy)))
            .map(|(gx, gy)| (out.get(gx, gy) - grid.get(gx, gy)).abs())
            .sum::<f32>()
            / (16 * 20) as f32;

        assert!(
            ramp_delta > plain_delta * 3.0,
            "steep ramp should get visibly more texture than the flat plain: plain={plain_delta}, ramp={ramp_delta}"
        );
    }

    #[test]
    fn detail_is_exactly_zero_inside_water_and_footprint() {
        let grid = plain_and_ramp_grid();
        let mut mask = WaterMask::new(grid.width, grid.height);
        for gy in 0..20 {
            mask.set(25, gy, true); // a strip of "water" right in the steep ramp
        }
        let mut footprint = FootprintMask::none(grid.width, grid.height);
        for gy in 0..20 {
            footprint.set(30, gy, true); // a strip of "capital footprint" also in the ramp
        }

        let out = run_detail_stage(&grid, &mask, &footprint, &detail_config(), grid.cell_size_m);
        for gy in 0..20 {
            assert_eq!(out.get(25, gy), grid.get(25, gy), "water cell must be untouched");
            assert_eq!(out.get(30, gy), grid.get(30, gy), "footprint cell must be untouched");
        }
        // Sanity: elsewhere in the same steep ramp, detail *did* apply.
        assert_ne!(out.get(28, 5), grid.get(28, 5));
    }

    #[test]
    fn detail_synthesis_is_deterministic() {
        let grid = plain_and_ramp_grid();
        let mask = WaterMask::new(grid.width, grid.height);
        let footprint = FootprintMask::none(grid.width, grid.height);
        let config = detail_config();
        let a = run_detail_stage(&grid, &mask, &footprint, &config, grid.cell_size_m);
        let b = run_detail_stage(&grid, &mask, &footprint, &config, grid.cell_size_m);
        assert_eq!(a, b);
        assert_eq!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn bicubic_upsample_reproduces_a_linear_ramp() {
        // Cubic interpolation is exact for a linear (degree-1) function --
        // a strong sanity check that the tap weights/offsets are right.
        // Restricted to the interior (away from the source grid's edges):
        // `bicubic_sample`'s 4-tap kernel clamps out-of-range taps to the
        // nearest edge cell rather than extrapolating the ramp, so points
        // needing a tap *before* the first or *after* the last source column
        // are a deliberate clamp-to-edge policy, not exact reconstruction --
        // constant-in-y here, so no equivalent restriction is needed on gy.
        let mut grid = Grid::new(10, 4, 10.0);
        for gy in 0..4 {
            for gx in 0..10 {
                grid.set(gx, gy, gx as f32 * 10.0); // height = world x
            }
        }
        let up = upsample_bicubic(&grid, 5.0); // 2x finer
        assert_eq!((up.width, up.height), (20, 8));
        for gy in 0..8 {
            for gx in 2..=15 {
                let expected = gx as f32 * 5.0;
                assert!(
                    (up.get(gx, gy) - expected).abs() < 0.01,
                    "({gx},{gy}): got {}, expected {expected}",
                    up.get(gx, gy)
                );
            }
        }
    }

    #[test]
    fn upsample_is_a_no_op_at_the_same_resolution() {
        let mut grid = Grid::new(4, 4, 10.0);
        grid.set(1, 1, 42.0);
        let out = upsample_bicubic(&grid, 10.0);
        assert_eq!(out, grid);
    }
}
