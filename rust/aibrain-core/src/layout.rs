//! Where every star sits.
//!
//! Ported from the Python `graph.py`, with one change that matters: a note's
//! position along its ribbon comes from a hash of `(brain_id, rel_path)`
//! rather than from its index in a sorted array. That single change is what
//! makes the rest possible — editing one note no longer reshuffles the galaxy
//! around it, so the cache invalidates per note instead of per vault, and
//! there is no longer any reason to cap how many notes a brain may draw.

use serde::{Deserialize, Serialize};

/// Folders that are containers rather than subjects get folded together, so a
/// vault does not become thirty near-empty ribbons.
const MIN_SOURCE_SHARE: f64 = 0.012;
const MAX_SOURCES: usize = 8;
pub const OTHER_LABEL: &str = "Other";

pub const SOURCE_COLORS: [&str; 10] = [
    "#4db3f0", "#f06aa6", "#f0c030", "#3ecf9a", "#9b7cf0",
    "#f2952d", "#4fd6d0", "#e0637a", "#7fd8e8", "#c0d05a",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceMeta {
    pub id: String,
    pub name: String,
    pub color: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainMeta {
    pub id: String,
    pub name: String,
    pub center: [f32; 3],
    pub radius: f32,
    pub seed: i32,
    pub sources: Vec<SourceMeta>,
    pub notes: usize,
    pub edges: usize,
    pub path: String,
}

/// A note as the layout needs it.
#[derive(Debug, Clone)]
pub struct LayoutNote {
    pub id: i64,
    pub rel_path: String,
    pub title: String,
    pub source: String,
    pub degree: i32,
    pub mtime: f64,
}

/// Where a note ended up.
#[derive(Debug, Clone)]
pub struct Placed {
    pub id: i64,
    pub position: [f32; 3],
    pub size: f32,
    pub source_index: u8,
    pub degree: i32,
}

/// A note's stable position along its ribbon, in `[0, 1)`.
///
/// Hashing the identity rather than counting an index is the whole trick: the
/// value depends on nothing but this note, so adding, removing or editing its
/// neighbours cannot move it.
pub fn slot_for(brain_id: &str, rel_path: &str) -> f64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(brain_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(rel_path.as_bytes());
    let bytes = hasher.finalize();
    let raw = u64::from_le_bytes(bytes.as_bytes()[..8].try_into().unwrap());
    // 53 bits keeps it exactly representable as an f64, which matters because
    // this value round-trips through Postgres as DOUBLE PRECISION.
    (raw >> 11) as f64 / (1u64 << 53) as f64
}

/// Round a count onto a geometric ladder, so it changes rarely and in steps.
///
/// Used for anything a note's position depends on that is not the note itself.
/// Growth of less than 25% leaves the value untouched, which is what keeps a
/// galaxy still while you edit it.
pub fn quantize(count: f64) -> f64 {
    if count <= 8.0 {
        return count;
    }
    const STEP: f64 = 1.25;
    STEP.powf((count.ln() / STEP.ln()).round())
}

/// Radius that keeps note density roughly constant across vaults.
///
/// Notes sit on a shell, so area — radius squared — has to grow with the count
/// or a large vault renders as one white blob.
pub fn brain_radius(note_count: usize) -> f32 {
    // Quantised for the same reason the ribbon span is: the radius scales every
    // position on the shell, so deriving it from the exact count would move
    // every star in the galaxy each time a single note was added.
    let n = quantize(note_count.max(1) as f64) as f32;
    (0.34 * n.sqrt() + 2.6).clamp(4.5, 17.0)
}

/// Lay the galaxies out so they all fit one screen without touching.
///
/// Up to three vaults read best as a row. Past that a row gets so wide the
/// camera has to pull back until every galaxy is a speck, so they stack.
pub fn brain_slots(radii: &[f32]) -> Vec<[f32; 3]> {
    match radii.len() {
        0 => return Vec::new(),
        1 => return vec![[0.0, 0.0, 0.0]],
        _ => {}
    }
    let count = radii.len();
    let rows = if count <= 3 { 1 } else if count <= 8 { 2 } else { 3 };
    let cols = count.div_ceil(rows);

    let cells: Vec<(usize, usize)> = (0..count).map(|i| (i / cols, i % cols)).collect();
    let col_r: Vec<f32> = (0..cols)
        .map(|c| {
            (0..count)
                .filter(|&i| cells[i].1 == c)
                .map(|i| radii[i])
                .fold(5.0f32, f32::max)
        })
        .collect();
    let row_r: Vec<f32> = (0..rows)
        .map(|r| {
            (0..count)
                .filter(|&i| cells[i].0 == r)
                .map(|i| radii[i])
                .fold(5.0f32, f32::max)
        })
        .collect();

    let axis = |sizes: &[f32], pad: f32| -> Vec<f32> {
        let mut out = Vec::with_capacity(sizes.len());
        let mut cursor = 0.0f32;
        for (i, size) in sizes.iter().enumerate() {
            if i > 0 {
                cursor += (sizes[i - 1] + size) * 1.1 + pad;
            }
            out.push(cursor);
        }
        let centre = (out[0] + out[out.len() - 1]) / 2.0;
        out.iter().map(|p| p - centre).collect()
    };

    let xs = axis(&col_r, 4.0);
    let ys = axis(&row_r, 2.5);

    cells
        .iter()
        .enumerate()
        .map(|(i, &(r, c))| {
            // Stagger depth so a grid does not read as a flat wall.
            let z = 0.5 * radii[i] * (i as f32 * 1.7).cos();
            [xs[c], -ys[r], z]
        })
        .collect()
}

/// Group notes by top-level folder, merging the long tail into "Other".
pub fn source_buckets(notes: &[LayoutNote], brain_name: &str) -> Vec<(String, Vec<usize>)> {
    let mut buckets: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, note) in notes.iter().enumerate() {
        match buckets.iter_mut().find(|(name, _)| *name == note.source) {
            Some((_, list)) => list.push(i),
            None => buckets.push((note.source.clone(), vec![i])),
        }
    }
    buckets.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    let total = notes.len() as f64;
    let floor = (total * MIN_SOURCE_SHARE).max(2.0);
    let mut keep: Vec<(String, Vec<usize>)> = Vec::new();
    let mut tail: Vec<usize> = Vec::new();
    for (name, items) in buckets {
        if keep.len() < MAX_SOURCES && items.len() as f64 >= floor {
            keep.push((name, items));
        } else {
            tail.extend(items);
        }
    }
    if !tail.is_empty() {
        keep.push((OTHER_LABEL.to_string(), tail));
    }
    if keep.is_empty() {
        keep.push((brain_name.to_string(), (0..notes.len()).collect()));
    }
    keep
}

/// A small deterministic PRNG, matching the one the renderer uses so that
/// positions computed here land where the design expects them.
struct Rng(u32);

impl Rng {
    fn new(seed: u32) -> Self {
        Rng(seed)
    }
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6D2B79F5);
        let mut t = self.0;
        t = (t ^ (t >> 15)).wrapping_mul(1 | t);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        ((t ^ (t >> 14)) as f64) / 4294967296.0
    }
    fn gauss(&mut self) -> f64 {
        (self.next() + self.next() + self.next() - 1.5) * 1.4
    }
}

/// Place every note in one brain onto its shell.
pub fn place_brain(
    brain_id: &str,
    brain_name: &str,
    seed: i32,
    notes: &[LayoutNote],
    ribbon_twist: f64,
) -> (Vec<Placed>, Vec<SourceMeta>, f32) {
    let radius = brain_radius(notes.len());
    let buckets = source_buckets(notes, brain_name);

    let mut rng = Rng::new(seed.unsigned_abs());
    let golden = std::f64::consts::PI * (3.0 - 5.0f64.sqrt());
    let base_rot = rng.next() * std::f64::consts::TAU;

    // Degree sets size, so a vault's hubs are literally its brightest stars.
    let mut degrees: Vec<i32> = notes.iter().map(|n| n.degree).collect();
    degrees.sort_unstable();
    let pct = |p: f64| -> i32 {
        if degrees.is_empty() {
            0
        } else {
            degrees[((degrees.len() as f64 * p) as usize).min(degrees.len() - 1)]
        }
    };
    let hub_cut = pct(0.965).max(3);
    let mid_cut = pct(0.72).max(2);
    let max_deg = (*degrees.last().unwrap_or(&1)).max(1) as f32;

    let mut placed = Vec::with_capacity(notes.len());
    let mut sources = Vec::with_capacity(buckets.len());

    for (si, (name, members)) in buckets.iter().enumerate() {
        let count = members.len().max(1) as f64;
        let y = 1.0 - (2.0 * (si as f64 + 0.5)) / buckets.len() as f64;
        let rad = (1.0 - y * y).max(0.0).sqrt();
        let th = base_rot + si as f64 * golden;

        let u = normalize3([th.cos() * rad, y, th.sin() * rad]);
        let tilt = normalize3([rng.next() - 0.5, rng.next() - 0.5, rng.next() - 0.5]);
        let w = normalize3(cross(u, tilt));
        let v = normalize3(cross(w, u));

        // A ribbon's arc grows with how many notes it holds — but if it grew
        // with the exact count, adding one note would re-span the arc and slide
        // every other note along it, defeating the stable slots entirely.
        // Quantising to a geometric ladder means the arc only changes when the
        // ribbon grows by a quarter, which during editing is never.
        let steps = quantize(count);
        let span = (0.6 + steps / 130.0).min(std::f64::consts::PI * 1.2);
        let width = 0.08 + (steps / 950.0).min(0.34);
        let t0 = -span / 2.0;
        let phase = rng.next() * std::f64::consts::TAU;

        for &idx in members {
            let note = &notes[idx];
            // The stable slot replaces "position in a sorted array".
            let slot = slot_for(brain_id, &note.rel_path);
            // A second independent hash gives this note its jitter and depth,
            // so those are stable too rather than drawn from a shared stream.
            let jitter = slot_for(&format!("{brain_id}#j"), &note.rel_path);

            let t = t0 + span * slot + (jitter - 0.5) * 0.08;
            let lat = (jitter - 0.5) * 2.0 * width * 1.4
                + ribbon_twist * 0.6 * (2.5 * t + phase).sin();
            let rr = if jitter < 0.75 {
                radius as f64 * (0.96 + jitter * 0.04)
            } else {
                radius as f64 * (0.78 + jitter * 0.18)
            };

            let (ct, st) = (t.cos(), t.sin());
            let (cl, sl) = (lat.cos(), lat.sin());
            let position = [
                ((u[0] * ct * cl) + (v[0] * st * cl) + (w[0] * sl)) * rr,
                ((u[1] * ct * cl) + (v[1] * st * cl) + (w[1] * sl)) * rr,
                ((u[2] * ct * cl) + (v[2] * st * cl) + (w[2] * sl)) * rr,
            ];

            let size = if note.degree >= hub_cut {
                0.5 + 0.45 * (note.degree as f32 / max_deg).min(1.0)
            } else if note.degree >= mid_cut {
                0.26 + 0.14 * jitter as f32
            } else {
                0.16 + 0.1 * jitter as f32
            };

            placed.push(Placed {
                id: note.id,
                position: [position[0] as f32, position[1] as f32, position[2] as f32],
                size,
                source_index: si.min(255) as u8,
                degree: note.degree,
            });
        }

        sources.push(SourceMeta {
            id: format!("{brain_id}:{}", slugify(name)),
            name: name.clone(),
            color: SOURCE_COLORS[(si + 3) % SOURCE_COLORS.len()].to_string(),
            count: members.len(),
        });
    }

    (placed, sources, radius)
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize3(v: [f64; 3]) -> [f64; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len < 1e-12 {
        [1.0, 0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}

pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "source".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: i64, path: &str, source: &str, degree: i32) -> LayoutNote {
        LayoutNote {
            id,
            rel_path: path.to_string(),
            title: path.to_string(),
            source: source.to_string(),
            degree,
            mtime: 0.0,
        }
    }

    #[test]
    fn a_slot_depends_only_on_the_note() {
        let a = slot_for("brain", "Notes/One.md");
        assert_eq!(a, slot_for("brain", "Notes/One.md"), "must be deterministic");
        assert_ne!(a, slot_for("other", "Notes/One.md"), "brain is part of it");
        assert_ne!(a, slot_for("brain", "Notes/Two.md"), "path is part of it");
        assert!((0.0..1.0).contains(&a));
    }

    #[test]
    fn slots_survive_a_round_trip_through_f64() {
        // Postgres stores this as DOUBLE PRECISION; a lossy value would move
        // stars every time the cache rebuilt.
        for path in ["a.md", "Journal/2026-01-01.md", "Very/Deep/Nested/Note.md"] {
            let slot = slot_for("b", path);
            assert_eq!(slot, f64::from_bits(slot.to_bits()));
        }
    }

    #[test]
    fn slots_spread_across_the_whole_ribbon() {
        // A biased hash would pile every note into one arc.
        let mut buckets = [0usize; 10];
        for i in 0..2000 {
            let slot = slot_for("brain", &format!("Notes/note-{i}.md"));
            buckets[(slot * 10.0) as usize] += 1;
        }
        for (i, count) in buckets.iter().enumerate() {
            assert!(
                (120..=280).contains(count),
                "decile {i} had {count} of 2000, expected roughly 200"
            );
        }
    }

    fn moved_count(before: &[Placed], after: &[Placed]) -> usize {
        before
            .iter()
            .filter(|b| {
                after
                    .iter()
                    .find(|a| a.id == b.id)
                    .map(|a| a.position != b.position)
                    .unwrap_or(true)
            })
            .count()
    }

    #[test]
    fn editing_one_note_moves_only_that_note() {
        // This is the property the whole design rests on: if a neighbour moves
        // when you edit a note, the layout cache cannot be incremental and the
        // galaxy visibly scrambles while you type in Obsidian.
        let before: Vec<LayoutNote> = (0..500)
            .map(|i| note(i, &format!("n{i}.md"), "Notes", 1))
            .collect();
        let (placed_before, _, _) = place_brain("b", "B", 7, &before, 0.25);

        let mut after = before.clone();
        after.push(note(9999, "new.md", "Notes", 1));
        let (placed_after, _, _) = place_brain("b", "B", 7, &after, 0.25);
        assert_eq!(
            moved_count(&placed_before, &placed_after),
            0,
            "adding a note must not move its neighbours"
        );

        // Deleting one is the same promise in reverse.
        let fewer: Vec<LayoutNote> = before[1..].to_vec();
        let (placed_fewer, _, _) = place_brain("b", "B", 7, &fewer, 0.25);
        assert_eq!(
            moved_count(&placed_fewer, &placed_before),
            0,
            "removing a note must not move the rest"
        );
    }

    #[test]
    fn a_ribbon_only_respans_after_real_growth() {
        // Quantising trades exactness for stillness: the arc is allowed to
        // change, but only once a ribbon has grown by about a quarter.
        assert_eq!(quantize(400.0), quantize(420.0));
        assert_ne!(quantize(400.0), quantize(700.0));
        // Small ribbons are exact, so a four-note vault still looks right.
        for n in 1..=8 {
            assert_eq!(quantize(n as f64), n as f64);
        }
        assert!(quantize(10_000.0) > quantize(1_000.0));
    }

    #[test]
    fn radius_grows_with_the_square_root_of_the_corpus() {
        assert!(brain_radius(4) < brain_radius(500));
        assert!(brain_radius(500) < brain_radius(2500));
        // Four notes must still be visible, and a huge vault must not eat the sky.
        assert_eq!(brain_radius(1), 4.5);
        assert_eq!(brain_radius(1_000_000), 17.0);
    }

    #[test]
    fn galaxies_never_overlap() {
        for count in 1..=12 {
            let radii: Vec<f32> = (0..count)
                .map(|i| brain_radius((i + 1) * 400))
                .collect();
            let slots = brain_slots(&radii);
            assert_eq!(slots.len(), count);
            for i in 0..count {
                for j in (i + 1)..count {
                    let d = (0..3)
                        .map(|k| (slots[i][k] - slots[j][k]).powi(2))
                        .sum::<f32>()
                        .sqrt();
                    assert!(
                        d > radii[i] + radii[j],
                        "with {count} brains, {i} and {j} overlap: gap {d}, radii {} + {}",
                        radii[i],
                        radii[j]
                    );
                }
            }
        }
    }

    #[test]
    fn the_long_tail_of_folders_becomes_one_ribbon() {
        let mut notes: Vec<LayoutNote> = (0..300)
            .map(|i| note(i, &format!("Notes/{i}.md"), "Notes", 1))
            .collect();
        for i in 0..12 {
            notes.push(note(1000 + i, &format!("Odd{i}/x.md"), &format!("Odd{i}"), 1));
        }
        let buckets = source_buckets(&notes, "B");
        assert_eq!(buckets[0].0, "Notes");
        assert!(buckets.iter().any(|(name, _)| name == OTHER_LABEL));
        assert!(buckets.len() <= MAX_SOURCES + 1);
        let total: usize = buckets.iter().map(|(_, m)| m.len()).sum();
        assert_eq!(total, notes.len(), "every note lands in exactly one ribbon");
    }

    #[test]
    fn every_note_is_placed_on_its_shell() {
        let notes: Vec<LayoutNote> = (0..400)
            .map(|i| note(i, &format!("Notes/{i}.md"), "Notes", i as i32 % 30))
            .collect();
        let (placed, sources, radius) = place_brain("b", "B", 11, &notes, 0.25);
        assert_eq!(placed.len(), notes.len());
        assert_eq!(sources.iter().map(|s| s.count).sum::<usize>(), notes.len());
        for p in &placed {
            let r = (p.position[0].powi(2) + p.position[1].powi(2) + p.position[2].powi(2)).sqrt();
            assert!(r <= radius * 1.02, "{r} escaped a shell of {radius}");
            assert!(r >= radius * 0.7, "{r} collapsed inside a shell of {radius}");
            assert!(p.size > 0.0);
        }
    }

    #[test]
    fn hubs_are_bigger_than_leaves() {
        let mut notes: Vec<LayoutNote> = (0..200)
            .map(|i| note(i, &format!("n{i}.md"), "Notes", 1))
            .collect();
        notes.push(note(999, "hub.md", "Notes", 400));
        let (placed, _, _) = place_brain("b", "B", 3, &notes, 0.25);
        let hub = placed.iter().find(|p| p.id == 999).unwrap();
        let leaf = placed.iter().find(|p| p.id == 0).unwrap();
        assert!(hub.size > leaf.size * 2.0, "hub {} vs leaf {}", hub.size, leaf.size);
    }
}
