//! Assembling the universe payload, and the cache underneath it.
//!
//! Replaces the Python `graph.build`. There is no per-brain node cap any more:
//! positions are stable per note, edges are fetched in one query per brain
//! instead of being chunked, and the browser gets the whole corpus.
//!
//! At 465 notes recomputing the layout on every page load cost 57 ms; at six
//! thousand it is four seconds, on every load, for a corpus that usually has
//! not moved. So each brain's packed geometry is stored in `graph_cache` and
//! keyed by its revision: a request rebuilds only the vaults whose notes
//! actually changed, and a request for a corpus that changed nothing at all
//! answers 304 from the ETag without touching the cache.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;

use crate::config::{BrainSpec, Config};
use crate::db;
use crate::layout;

/// Cross-brain arcs are drawn per frame in world space, so they cost more than
/// an ordinary edge. This many reads as "these vaults are connected" without
/// turning the sky into a web.
const MAX_CROSS: i64 = 400;

// ---------------------------------------------------------------------------
// the cache key
// ---------------------------------------------------------------------------

/// Whether a cached block can be served as it stands.
///
/// Two things can stale it. The revision covers everything about the notes,
/// and the signature covers the knobs that shape the layout without touching
/// a note — the ribbon twist, the seed, the vault's display name. Both are
/// stored with the block, so this stays a pure comparison.
pub fn cache_is_current(cached: Option<(i64, &str)>, revision: i64, signature: &str) -> bool {
    matches!(cached, Some((rev, sig)) if rev == revision && sig == signature)
}

/// The layout-shaping inputs that are not notes, as one comparable string.
pub fn signature(spec: &BrainSpec, ribbon_twist: f64) -> String {
    // Six decimals is finer than the slider can move and coarser than float
    // noise, so an unchanged config always produces an unchanged string.
    format!("{}|{}|{:.6}", spec.seed, spec.name, ribbon_twist)
}

/// The ETag for a universe: a hash of every `(brain_id, revision)` pair.
///
/// `shape` folds in the things that are not per-brain — the corpus title and
/// the ribbon twist — because a payload that changed for one of those reasons
/// must not answer 304 to a browser holding the old one.
pub fn etag(revisions: &[(String, i64)], shape: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(shape.as_bytes());
    hasher.update(b"\n");
    for (id, revision) in revisions {
        hasher.update(id.as_bytes());
        hasher.update(b"\0");
        hasher.update(&revision.to_le_bytes());
        hasher.update(b"\n");
    }
    // Sixteen bytes is far more than enough to tell two corpus states apart,
    // and keeps the header short enough to read in a log line.
    format!("\"{}\"", &hasher.finalize().to_hex().as_str()[..32])
}

/// Does an `If-None-Match` header match what we would send?
///
/// A cache may weaken a tag on the way through, and a browser may send a list,
/// so compare against each entry with any `W/` prefix stripped.
pub fn etag_matches(header: &str, current: &str) -> bool {
    let want = current.trim_start_matches("W/").trim();
    header.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.trim_start_matches("W/").trim() == want
    })
}

/// The `(brain_id, revision)` pairs behind a universe, in config order.
///
/// One small query. This is what makes a 304 cheap: the expensive part of
/// `/universe` never runs when the answer is "nothing moved".
pub async fn revisions(pool: &PgPool, cfg: &Config) -> Result<Vec<(String, i64)>> {
    let rows = db::list_brains(pool).await?;
    Ok(cfg
        .brains
        .iter()
        .filter_map(|spec| {
            rows.iter()
                .find(|row| row.id == spec.id)
                .map(|row| (spec.id.clone(), row.revision))
        })
        .collect())
}

/// The corpus-wide half of the ETag input.
pub fn shape_of(cfg: &Config) -> String {
    format!("{}|{:.6}", cfg.title, cfg.ribbon_twist)
}

// ---------------------------------------------------------------------------
// the packed block
// ---------------------------------------------------------------------------

/// `AIB1`, so a row written by an older build is recognised and discarded
/// rather than decoded into nonsense.
const MAGIC: u32 = 0x3142_4941;

/// One brain's geometry: everything the browser uploads into GPU attributes,
/// plus the note ids that let the rest of the payload refer to them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Geometry {
    pub note_ids: Vec<i64>,
    pub positions: Vec<f32>,
    pub sizes: Vec<f32>,
    pub degrees: Vec<i32>,
    pub source_index: Vec<u8>,
    pub edges: Vec<[u32; 2]>,
}

impl Geometry {
    pub fn nodes(&self) -> usize {
        self.note_ids.len()
    }
}

/// Pack a brain's geometry little-endian, framed by a 16-byte header of
/// magic, version, node count and edge count.
pub fn pack(geometry: &Geometry) -> Vec<u8> {
    let n = geometry.nodes();
    let e = geometry.edges.len();
    let mut out = Vec::with_capacity(16 + n * 29 + e * 8);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.extend_from_slice(&(e as u32).to_le_bytes());
    for value in &geometry.positions {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in &geometry.sizes {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in &geometry.degrees {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in &geometry.note_ids {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&geometry.source_index);
    for [a, b] in &geometry.edges {
        out.extend_from_slice(&a.to_le_bytes());
        out.extend_from_slice(&b.to_le_bytes());
    }
    out
}

/// The inverse of `pack`. Anything it cannot make sense of is an error, which
/// the caller treats as a cache miss rather than a failure.
pub fn unpack(bytes: &[u8]) -> Result<Geometry> {
    let head = |at: usize| -> Result<u32> {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .ok_or_else(|| anyhow!("graph_cache row is truncated"))
    };
    if head(0)? != MAGIC {
        return Err(anyhow!("graph_cache row is not AIB1"));
    }
    if head(4)? != 1 {
        return Err(anyhow!("graph_cache row is a future version"));
    }
    let n = head(8)? as usize;
    let e = head(12)? as usize;

    let expected = 16 + n * 12 + n * 4 + n * 4 + n * 8 + n + e * 8;
    if bytes.len() != expected {
        return Err(anyhow!("graph_cache row is {} bytes, expected {expected}", bytes.len()));
    }

    let mut at = 16;
    let positions = bytes[at..at + n * 12]
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    at += n * 12;
    let sizes = bytes[at..at + n * 4]
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    at += n * 4;
    let degrees = bytes[at..at + n * 4]
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    at += n * 4;
    let note_ids = bytes[at..at + n * 8]
        .chunks_exact(8)
        .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
        .collect();
    at += n * 8;
    let source_index = bytes[at..at + n].to_vec();
    at += n;
    let edges = bytes[at..at + e * 8]
        .chunks_exact(8)
        .map(|b| {
            [
                u32::from_le_bytes(b[0..4].try_into().unwrap()),
                u32::from_le_bytes(b[4..8].try_into().unwrap()),
            ]
        })
        .collect();

    Ok(Geometry { note_ids, positions, sizes, degrees, source_index, edges })
}

/// The half of a brain's block that stays JSON: strings, and the numbers the
/// browser reads once rather than uploading.
#[derive(Debug, Clone)]
struct Meta {
    signature: String,
    radius: f32,
    names: Vec<String>,
    sources: Value,
}

impl Meta {
    fn to_json(&self) -> Value {
        json!({
            "sig": self.signature,
            "radius": self.radius,
            "names": self.names,
            "sources": self.sources,
        })
    }

    fn from_json(value: &Value) -> Option<Meta> {
        Some(Meta {
            signature: value.get("sig")?.as_str()?.to_string(),
            radius: value.get("radius")?.as_f64()? as f32,
            names: value
                .get("names")?
                .as_array()?
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect(),
            sources: value.get("sources")?.clone(),
        })
    }
}

/// A brain's geometry, from the cache when it is current and from the layout
/// when it is not.
async fn block_for(
    pool: &PgPool,
    spec: &BrainSpec,
    revision: i64,
    ribbon_twist: f64,
    rebuilt: &mut Vec<String>,
) -> Result<(Geometry, Meta)> {
    let signature = signature(spec, ribbon_twist);

    if let Some(cached) = db::graph_cache_get(pool, &spec.id).await? {
        if let Some(meta) = Meta::from_json(&cached.meta) {
            if cache_is_current(Some((cached.revision, &meta.signature)), revision, &signature) {
                match unpack(&cached.geometry) {
                    Ok(geometry) if geometry.nodes() == meta.names.len() => {
                        return Ok((geometry, meta))
                    }
                    Ok(_) => tracing::warn!("{}: cached names and nodes disagree", spec.id),
                    // A corrupt row is a rebuild, not a 500.
                    Err(err) => tracing::warn!("{}: {err:#}", spec.id),
                }
            }
        }
    }

    let notes = db::notes_for_layout(pool, &spec.id).await?;
    let (placed, sources, radius) =
        layout::place_brain(&spec.id, &spec.name, spec.seed, &notes, ribbon_twist);

    let mut geometry = Geometry {
        note_ids: Vec::with_capacity(placed.len()),
        positions: Vec::with_capacity(placed.len() * 3),
        sizes: Vec::with_capacity(placed.len()),
        degrees: Vec::with_capacity(placed.len()),
        source_index: Vec::with_capacity(placed.len()),
        edges: Vec::new(),
    };
    let mut local_of: HashMap<i64, u32> = HashMap::with_capacity(placed.len());
    for node in &placed {
        local_of.insert(node.id, geometry.note_ids.len() as u32);
        geometry.note_ids.push(node.id);
        geometry.positions.extend_from_slice(&node.position);
        geometry.sizes.push(node.size);
        geometry.degrees.push(node.degree);
        geometry.source_index.push(node.source_index);
    }
    geometry.edges = db::edges_within(pool, &spec.id)
        .await?
        .iter()
        .filter_map(|(a, b)| Some([*local_of.get(a)?, *local_of.get(b)?]))
        .collect();

    // Titles come back in the same order as the placement, so the browser can
    // index them by node without a second lookup.
    let names = db::titles_for(pool, &geometry.note_ids).await?;
    let meta = Meta {
        signature,
        radius,
        names,
        sources: serde_json::to_value(&sources)?,
    };

    db::graph_cache_put(
        pool,
        &spec.id,
        revision,
        geometry.nodes() as i32,
        geometry.edges.len() as i32,
        &pack(&geometry),
        &meta.to_json(),
    )
    .await?;
    rebuilt.push(spec.id.clone());

    Ok((geometry, meta))
}

/// The whole payload. `revisions` is passed in rather than read again so the
/// ETag the caller already computed describes exactly what is built here.
pub async fn build(pool: &PgPool, cfg: &Config, revisions: &[(String, i64)]) -> Result<Value> {
    let by_id: HashMap<&str, i64> =
        revisions.iter().map(|(id, rev)| (id.as_str(), *rev)).collect();
    // Config order decides layout order, so the chips and the galaxies agree.
    let ordered: Vec<&BrainSpec> = cfg
        .brains
        .iter()
        .filter(|spec| by_id.contains_key(spec.id.as_str()))
        .collect();

    let mut rebuilt: Vec<String> = Vec::new();
    let mut blocks = Vec::with_capacity(ordered.len());
    for spec in &ordered {
        let revision = by_id[spec.id.as_str()];
        let (geometry, meta) =
            block_for(pool, spec, revision, cfg.ribbon_twist, &mut rebuilt).await?;
        blocks.push((*spec, revision, geometry, meta));
    }
    if !rebuilt.is_empty() {
        tracing::debug!("universe: rebuilt {}", rebuilt.join(", "));
    }

    let radii: Vec<f32> = blocks.iter().map(|(_, _, _, meta)| meta.radius).collect();
    let slots = layout::brain_slots(&radii);

    // Global ids are assigned by counting, and the browser maps them straight
    // back to note ids — so this ordering is a contract, not an implementation
    // detail. Node i in the payload is note_ids[i].
    let mut note_ids: Vec<i64> = Vec::new();
    let mut gid_of: HashMap<i64, usize> = HashMap::new();
    let mut brains_out: Vec<Value> = Vec::with_capacity(blocks.len());

    for (index, (spec, revision, geometry, meta)) in blocks.iter().enumerate() {
        for id in &geometry.note_ids {
            gid_of.insert(*id, note_ids.len());
            note_ids.push(*id);
        }
        brains_out.push(json!({
            "id": spec.id,
            "name": spec.name,
            "center": slots[index],
            "radius": meta.radius,
            "seed": spec.seed,
            "revision": revision,
            "path": spec.root,
            "total": geometry.nodes(),
            "sources": meta.sources,
            "positions": geometry.positions,
            "sizes": geometry.sizes,
            "sourceIndex": geometry.source_index,
            "degrees": geometry.degrees,
            "names": meta.names,
            "edges": geometry.edges,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, name: &str, seed: i32) -> BrainSpec {
        BrainSpec {
            id: id.into(),
            name: name.into(),
            root: format!("/vaults/{id}"),
            seed,
            excludes: vec![],
        }
    }

    // ---- the etag ------------------------------------------------------

    #[test]
    fn the_same_revisions_hash_to_the_same_tag() {
        let pairs = vec![("a".to_string(), 3), ("b".to_string(), 9)];
        assert_eq!(etag(&pairs, "t|0.250000"), etag(&pairs, "t|0.250000"));
    }

    #[test]
    fn one_brain_moving_changes_the_tag() {
        let before = vec![("a".to_string(), 3), ("b".to_string(), 9)];
        let after = vec![("a".to_string(), 3), ("b".to_string(), 10)];
        assert_ne!(etag(&before, "t"), etag(&after, "t"));
    }

    #[test]
    fn a_brain_appearing_or_leaving_changes_the_tag() {
        let one = vec![("a".to_string(), 3)];
        let two = vec![("a".to_string(), 3), ("b".to_string(), 0)];
        assert_ne!(etag(&one, "t"), etag(&two, "t"));
        assert_ne!(etag(&two, "t"), etag(&Vec::new(), "t"));
    }

    #[test]
    fn the_ids_are_separated_so_they_cannot_run_together() {
        // Without the separator "ab" + rev 1 and "a" + "b" + rev 1 would
        // collide, and two differently-named vaults would share a cache entry.
        let joined = vec![("ab".to_string(), 1)];
        let split = vec![("a".to_string(), 1), ("b".to_string(), 1)];
        assert_ne!(etag(&joined, "t"), etag(&split, "t"));
    }

    #[test]
    fn the_shape_is_part_of_the_tag() {
        let pairs = vec![("a".to_string(), 1)];
        assert_ne!(etag(&pairs, "Title|0.250000"), etag(&pairs, "Title|0.400000"));
    }

    #[test]
    fn the_tag_is_a_quoted_hex_string() {
        let tag = etag(&[("a".to_string(), 1)], "t");
        assert!(tag.starts_with('"') && tag.ends_with('"'), "{tag}");
        assert_eq!(tag.len(), 34);
        assert!(tag[1..33].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn if_none_match_tolerates_weakness_and_lists() {
        let tag = etag(&[("a".to_string(), 1)], "t");
        assert!(etag_matches(&tag, &tag));
        assert!(etag_matches(&format!("W/{tag}"), &tag));
        assert!(etag_matches(&format!("\"other\", {tag}"), &tag));
        assert!(etag_matches("*", &tag));
        assert!(!etag_matches("\"other\"", &tag));
        assert!(!etag_matches("", &tag));
    }

    // ---- hit or miss ---------------------------------------------------

    #[test]
    fn a_matching_revision_and_signature_is_a_hit() {
        assert!(cache_is_current(Some((7, "sig")), 7, "sig"));
    }

    #[test]
    fn a_moved_revision_is_a_miss() {
        assert!(!cache_is_current(Some((7, "sig")), 8, "sig"));
        // And so is a revision that went backwards, which is what a dropped
        // and re-created brain looks like.
        assert!(!cache_is_current(Some((8, "sig")), 7, "sig"));
    }

    #[test]
    fn a_changed_knob_is_a_miss_even_at_the_same_revision() {
        let twisted = signature(&spec("a", "A", 7), 0.25);
        let straighter = signature(&spec("a", "A", 7), 0.4);
        assert_ne!(twisted, straighter);
        assert!(!cache_is_current(Some((7, &twisted)), 7, &straighter));
    }

    #[test]
    fn a_renamed_or_reseeded_vault_is_a_miss() {
        let before = signature(&spec("a", "A", 7), 0.25);
        assert!(!cache_is_current(Some((1, &before)), 1, &signature(&spec("a", "B", 7), 0.25)));
        assert!(!cache_is_current(Some((1, &before)), 1, &signature(&spec("a", "A", 8), 0.25)));
    }

    #[test]
    fn nothing_cached_is_a_miss() {
        assert!(!cache_is_current(None, 0, "sig"));
    }

    // ---- packing -------------------------------------------------------

    fn sample() -> Geometry {
        Geometry {
            note_ids: vec![11, 12, 13],
            positions: vec![0.5, -1.5, 2.0, 3.25, 4.0, -5.5, 0.0, 0.0, 0.125],
            sizes: vec![0.2, 0.35, 1.0],
            degrees: vec![0, 4, 17],
            source_index: vec![0, 2, 1],
            edges: vec![[0, 1], [1, 2]],
        }
    }

    #[test]
    fn a_packed_brain_unpacks_to_itself() {
        let packed = pack(&sample());
        assert_eq!(unpack(&packed).unwrap(), sample());
    }

    #[test]
    fn an_empty_brain_packs_to_just_a_header() {
        let packed = pack(&Geometry::default());
        assert_eq!(packed.len(), 16);
        assert_eq!(unpack(&packed).unwrap(), Geometry::default());
    }

    #[test]
    fn a_truncated_or_foreign_row_is_an_error_not_a_panic() {
        let packed = pack(&sample());
        assert!(unpack(&packed[..packed.len() - 4]).is_err());
        assert!(unpack(&[]).is_err());
        assert!(unpack(b"not a cache row at all").is_err());
        let mut wrong_version = packed.clone();
        wrong_version[4] = 9;
        assert!(unpack(&wrong_version).is_err());
    }

    #[test]
    fn the_header_reports_what_the_body_holds() {
        let packed = pack(&sample());
        assert_eq!(u32::from_le_bytes(packed[8..12].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(packed[12..16].try_into().unwrap()), 2);
    }
}
