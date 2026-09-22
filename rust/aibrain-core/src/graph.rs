//! Assembling the universe payload.
//!
//! Replaces the Python `graph.build`. There is no per-brain node cap any more:
//! positions are stable per note, edges are fetched in one query per brain
//! instead of being chunked, and the browser gets the whole corpus.

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;

use crate::config::Config;
use crate::db;
use crate::layout;

/// Cross-brain arcs are drawn per frame in world space, so they cost more than
/// an ordinary edge. This many reads as "these vaults are connected" without
/// turning the sky into a web.
const MAX_CROSS: i64 = 400;

pub async fn build(pool: &PgPool, cfg: &Config) -> Result<Value> {
    let rows = db::list_brains(pool).await?;
    // Config order decides layout order, so the chips and the galaxies agree.
    let ordered: Vec<_> = cfg
        .brains
        .iter()
        .filter_map(|spec| rows.iter().find(|r| r.id == spec.id).map(|r| (spec, r)))
        .collect();

    let mut per_brain = Vec::with_capacity(ordered.len());
    for (spec, row) in &ordered {
        let notes = db::notes_for_layout(pool, &spec.id).await?;
        let (placed, sources, radius) =
            layout::place_brain(&spec.id, &spec.name, spec.seed, &notes, cfg.ribbon_twist);
        per_brain.push((spec, row, placed, sources, radius));
    }

    let radii: Vec<f32> = per_brain.iter().map(|(_, _, _, _, r)| *r).collect();
    let slots = layout::brain_slots(&radii);

    // Global ids are assigned by counting, and the browser maps them straight
    // back to note ids — so this ordering is a contract, not an implementation
    // detail. Node i in the payload is note_ids[i].
    let mut note_ids: Vec<i64> = Vec::new();
    let mut gid_of: HashMap<i64, usize> = HashMap::new();
    let mut brains_out: Vec<Value> = Vec::new();

    for (index, (spec, row, placed, sources, radius)) in per_brain.iter().enumerate() {
        let base = note_ids.len();
        let mut positions: Vec<f32> = Vec::with_capacity(placed.len() * 3);
        let mut sizes: Vec<f32> = Vec::with_capacity(placed.len());
        let mut source_index: Vec<u8> = Vec::with_capacity(placed.len());
        let mut degrees: Vec<i32> = Vec::with_capacity(placed.len());
        let mut names: Vec<&str> = Vec::with_capacity(placed.len());

        let by_id: HashMap<i64, &layout::LayoutNote> = HashMap::new();
        let _ = by_id;

        for node in placed.iter() {
            gid_of.insert(node.id, note_ids.len());
            note_ids.push(node.id);
            positions.extend_from_slice(&node.position);
            sizes.push(node.size);
            source_index.push(node.source_index);
            degrees.push(node.degree);
        }

        // Titles come back in the same order as the placement, so the browser
        // can index them by global id without a second lookup.
        let titles = db::titles_for(pool, &note_ids[base..]).await?;
        names.extend(titles.iter().map(String::as_str));

        let edges = db::edges_within(pool, &spec.id).await?;
        let local_edges: Vec<[u32; 2]> = edges
            .iter()
            .filter_map(|(a, b)| {
                Some([
                    (*gid_of.get(a)? - base) as u32,
                    (*gid_of.get(b)? - base) as u32,
                ])
            })
            .collect();

        brains_out.push(json!({
            "id": spec.id,
            "name": spec.name,
            "center": slots[index],
            "radius": radius,
            "seed": spec.seed,
            "revision": row.revision,
            "path": spec.root,
            "total": placed.len(),
            "sources": sources,
            "positions": positions,
            "sizes": sizes,
            "sourceIndex": source_index,
            "degrees": degrees,
            "names": titles,
            "edges": local_edges,
        }));
    }

    let cross: Vec<[u32; 2]> = db::edges_across(pool, MAX_CROSS)
        .await?
        .iter()
        .filter_map(|(a, b)| Some([*gid_of.get(a)? as u32, *gid_of.get(b)? as u32]))
        .collect();

    let extent_x = brains_out
        .iter()
        .map(|b| b["center"][0].as_f64().unwrap_or(0.0).abs() + b["radius"].as_f64().unwrap_or(0.0))
        .fold(20.0f64, f64::max);
    let extent_y = brains_out
        .iter()
        .map(|b| b["center"][1].as_f64().unwrap_or(0.0).abs() + b["radius"].as_f64().unwrap_or(0.0))
        .fold(14.0f64, f64::max);

    Ok(json!({
        "title": cfg.title,
        "brains": brains_out,
        "cross": cross,
        "noteIds": note_ids,
        "fitWidth": extent_x * 2.0 + 20.0,
        "fitHeight": extent_y * 2.0 + 20.0,
        "options": { "ribbonTwist": cfg.ribbon_twist },
        "stats": {
            "notes": note_ids.len(),
            "brains": brains_out.len(),
            "cross": cross.len(),
        },
    }))
}
