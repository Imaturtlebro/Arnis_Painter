use crate::args::Args;
use crate::coordinate_system::cartesian::XZBBox;
use crate::coordinate_system::geographic::LLBBox;
use crate::element_processing::*;
use crate::floodfill_cache::FloodFillCache;
use crate::ground::Ground;
use crate::ground_generation;
use crate::osm_parser::{ProcessedElement, ProcessedMemberRole};
use crate::progress::{emit_gui_progress_update, emit_show_in_folder};
use crate::world_editor::{WorldEditor, WorldFormat};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;
use std::sync::Arc;

/// Generation options that can be passed separately from CLI Args
#[derive(Clone)]
pub struct GenerationOptions {
    pub path: PathBuf,
    pub format: WorldFormat,
    pub level_name: Option<String>,
    pub spawn_point: Option<(i32, i32)>,
}

/// Generate world with explicit format options (used by GUI for Bedrock support)
pub fn generate_world_with_options(
    elements: Vec<ProcessedElement>,
    xzbbox: XZBBox,
    llbbox: LLBBox,
    ground: Ground,
    args: &Args,
    options: GenerationOptions,
) -> Result<PathBuf, String> {
    let output_path = options.path.clone();
    let world_format = options.format;
    let generation_start = args.benchmark.then(std::time::Instant::now);

    // Create editor with appropriate format
    let mut editor: WorldEditor = WorldEditor::new_with_format_and_name(
        options.path,
        &xzbbox,
        llbbox,
        options.format,
        options.level_name.clone(),
        options.spawn_point,
    );
    let ground = Arc::new(ground);

    println!("{} Processing data...", "[4/7]".bold());

    // Build highway connectivity map once before processing
    let highway_connectivity = highways::build_highway_connectivity_map(&elements);

    // Collect subway centerline points for post-ground-fill air carving (phase 2).
    let mut subway_points: Vec<(i32, i32)> = Vec::new();

    // Set ground reference in the editor to enable elevation-aware block placement
    editor.set_ground(Arc::clone(&ground));

    println!("{} Processing terrain...", "[5/7]".bold());
    emit_gui_progress_update(25.0, "Processing terrain...");

    // Pre-compute all flood fills in parallel for better CPU utilization
    let mut flood_fill_cache = FloodFillCache::precompute(&elements, args.timeout.as_ref());

    // Collect building footprints to prevent trees from spawning inside buildings
    // Uses a memory-efficient bitmap (~1 bit per coordinate) instead of a HashSet (~24 bytes per coordinate)
    let building_footprints = flood_fill_cache.collect_building_footprints(&elements, &xzbbox);

    // Collect coordinates covered by tunnel=building_passage highways so that
    // building generation can cut ground-level openings through walls and floors.
    let building_passages =
        highways::collect_building_passage_coords(&elements, &xzbbox, args.scale);

    // Pre-build a bitmap of every (x, z) block coordinate covered by a rendered
    // road or path surface. Uses the same Bresenham + block_range geometry as
    // generate_highways_internal, so the bitmap is a 1:1 match of what gets placed.
    // Amenity processors use this for O(1) nearest-road-block lookups.
    // TODO Use this data to create overhanging traffic signals.
    let road_mask = highways::collect_road_surface_coords(&elements, &xzbbox, args.scale);

    // Process all elements (no longer need to partition boundaries)
    let elements_count: usize = elements.len();
    let process_pb: ProgressBar = ProgressBar::new(elements_count as u64);
    process_pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:45.white/black}] {pos}/{len} elements ({eta}) {msg}")
        .unwrap()
        .progress_chars("█▓░"));

    let progress_increment_prcs: f64 = 45.0 / elements_count as f64;
    let mut current_progress_prcs: f64 = 25.0;
    let mut last_emitted_progress: f64 = current_progress_prcs;

    // Pre-scan: detect building relation outlines that should be suppressed.
    // Only applies to type=building relations (NOT type=multipolygon).
    // When a type=building relation has "part" members, the outline way should not
    // render as a standalone building, the individual parts render instead.
    let suppressed_building_outlines: HashSet<u64> = {
        let mut outlines = HashSet::new();
        for element in &elements {
            if let ProcessedElement::Relation(rel) = element {
                let is_building_type = rel.tags.get("type").map(|t| t.as_str()) == Some("building");
                if is_building_type {
                    let has_parts = rel
                        .members
                        .iter()
                        .any(|m| m.role == ProcessedMemberRole::Part);
                    if has_parts {
                        for member in &rel.members {
                            if member.role == ProcessedMemberRole::Outer {
                                outlines.insert(member.way.id);
                            }
                        }
                    }
                }
            }
        }
        outlines
    };

    // Process all elements
    for element in elements.into_iter() {
        process_pb.inc(1);
        current_progress_prcs += progress_increment_prcs;
        if (current_progress_prcs - last_emitted_progress).abs() > 0.25 {
            emit_gui_progress_update(current_progress_prcs, "");
            last_emitted_progress = current_progress_prcs;
        }

        if args.debug {
            process_pb.set_message(format!(
                "(Element ID: {} / Type: {})",
                element.id(),
                element.kind()
            ));
        } else {
            process_pb.set_message("");
        }

        match &element {
            ProcessedElement::Way(way) => {
                if way.tags.contains_key("building") || way.tags.contains_key("building:part") {
                    // Skip building outlines that are suppressed by building relations with parts.
                    // The individual building:part ways will render instead.
                    if !suppressed_building_outlines.contains(&way.id) {
                        buildings::generate_buildings(
                            &mut editor,
                            way,
                            args,
                            None,
                            None,
                            &flood_fill_cache,
                            &building_passages,
                        );
                    }
                } else if way.tags.contains_key("highway") {
                    highways::generate_highways(
                        &mut editor,
                        &element,
                        args,
                        &highway_connectivity,
                        &flood_fill_cache,
                    );
                } else if way.tags.contains_key("landuse") {
                    landuse::generate_landuse(
                        &mut editor,
                        way,
                        args,
                        &flood_fill_cache,
                        &building_footprints,
                    );
                } else if way.tags.contains_key("natural") {
                    natural::generate_natural(
                        &mut editor,
                        &element,
                        args,
                        &flood_fill_cache,
                        &building_footprints,
                    );
                } else if way.tags.contains_key("amenity") {
                    amenities::generate_amenities(
                        &mut editor,
                        &element,
                        args,
                        &flood_fill_cache,
                        &road_mask,
                    );
                } else if way.tags.contains_key("leisure") {
                    leisure::generate_leisure(
                        &mut editor,
                        way,
                        args,
                        &flood_fill_cache,
                        &building_footprints,
                    );
                } else if way.tags.contains_key("barrier") {
                    barriers::generate_barriers(&mut editor, &element);
                } else if let Some(val) = way.tags.get("waterway") {
                    if val == "dock" {
                        // docks count as water areas
                        water_areas::generate_water_area_from_way(&mut editor, way, &xzbbox);
                    } else {
                        waterways::generate_waterways(&mut editor, way);
                    }
                } else if way.tags.contains_key("bridge") {
                    //bridges::generate_bridges(&mut editor, way, ground_level); // TODO FIX
                } else if way.tags.contains_key("railway") {
                    railways::generate_railways(&mut editor, way, &mut subway_points);
                } else if way.tags.contains_key("roller_coaster") {
                    railways::generate_roller_coaster(&mut editor, way);
                } else if way.tags.contains_key("aeroway") || way.tags.contains_key("area:aeroway")
                {
                    highways::generate_aeroway(&mut editor, way, args);
                } else if way.tags.get("service") == Some(&"siding".to_string()) {
                    highways::generate_siding(&mut editor, way);
                } else if way.tags.get("tomb") == Some(&"pyramid".to_string()) {
                    historic::generate_pyramid(&mut editor, way, args, &flood_fill_cache);
                } else if way.tags.contains_key("man_made") {
                    man_made::generate_man_made(&mut editor, &element, args);
                } else if way.tags.contains_key("power") {
                    power::generate_power(&mut editor, &element);
                } else if way.tags.contains_key("place") {
                    landuse::generate_place(&mut editor, way, args, &flood_fill_cache);
                }
                // Release flood fill cache entry for this way
                flood_fill_cache.remove_way(way.id);
            }
            ProcessedElement::Node(node) => {
                if node.tags.contains_key("door") || node.tags.contains_key("entrance") {
                    doors::generate_doors(&mut editor, node);
                } else if node.tags.contains_key("natural")
                    && node.tags.get("natural") == Some(&"tree".to_string())
                {
                    natural::generate_natural(
                        &mut editor,
                        &element,
                        args,
                        &flood_fill_cache,
                        &building_footprints,
                    );
                } else if node.tags.contains_key("amenity") {
                    amenities::generate_amenities(
                        &mut editor,
                        &element,
                        args,
                        &flood_fill_cache,
                        &road_mask,
                    );
                } else if node.tags.contains_key("barrier") {
                    barriers::generate_barrier_nodes(&mut editor, node);
                } else if node.tags.contains_key("highway") {
                    highways::generate_highways(
                        &mut editor,
                        &element,
                        args,
                        &highway_connectivity,
                        &flood_fill_cache,
                    );
                } else if node.tags.contains_key("tourism") {
                    tourisms::generate_tourisms(&mut editor, node);
                } else if node.tags.contains_key("man_made") {
                    man_made::generate_man_made_nodes(&mut editor, node);
                } else if node.tags.contains_key("power") {
                    power::generate_power_nodes(&mut editor, node);
                } else if node.tags.contains_key("historic") {
                    historic::generate_historic(&mut editor, node);
                } else if node.tags.contains_key("emergency") {
                    emergency::generate_emergency(&mut editor, node);
                } else if node.tags.contains_key("advertising") {
                    advertising::generate_advertising(&mut editor, node);
                }
            }
            ProcessedElement::Relation(rel) => {
                let is_building_relation = rel.tags.contains_key("building")
                    || rel.tags.contains_key("building:part")
                    || rel.tags.get("type").map(|t| t.as_str()) == Some("building");
                if is_building_relation {
                    buildings::generate_building_from_relation(
                        &mut editor,
                        rel,
                        args,
                        &flood_fill_cache,
                        &xzbbox,
                        &building_passages,
                    );
                } else if rel.tags.contains_key("water")
                    || rel
                        .tags
                        .get("natural")
                        .map(|val| val == "water" || val == "bay")
                        .unwrap_or(false)
                {
                    water_areas::generate_water_areas_from_relation(&mut editor, rel, &xzbbox);
                } else if rel.tags.contains_key("natural") {
                    natural::generate_natural_from_relation(
                        &mut editor,
                        rel,
                        args,
                        &flood_fill_cache,
                        &building_footprints,
                    );
                } else if rel.tags.contains_key("landuse") {
                    landuse::generate_landuse_from_relation(
                        &mut editor,
                        rel,
                        args,
                        &flood_fill_cache,
                        &building_footprints,
                    );
                } else if rel.tags.get("leisure") == Some(&"park".to_string()) {
                    leisure::generate_leisure_from_relation(
                        &mut editor,
                        rel,
                        args,
                        &flood_fill_cache,
                        &building_footprints,
                    );
                } else if rel.tags.contains_key("man_made") {
                    man_made::generate_man_made(&mut editor, &element, args);
                }
                // Release flood fill cache entries for all ways in this relation
                let way_ids: Vec<u64> = rel.members.iter().map(|m| m.way.id).collect();
                flood_fill_cache.remove_relation_ways(&way_ids);
            }
        }
        // Element is dropped here, freeing its memory immediately
    }

    process_pb.finish();

    // Drop remaining caches
    drop(highway_connectivity);
    drop(flood_fill_cache);
    drop(road_mask);

    // Generate ground layer (surface blocks, vegetation, shorelines, underground fill)
    ground_generation::generate_ground_layer(
        &mut editor,
        ground.as_ref(),
        args,
        &xzbbox,
        &building_footprints,
    )?;

    // Carve subway tunnel interiors now that underground is filled with stone.
    // This must happen after ground generation so AIR blocks are not overwritten.
    if !subway_points.is_empty() {
        railways::carve_subway_interior(&mut editor, &subway_points);
    }

    // Save world
    if let Err(e) = editor.save() {
        return Err(e.to_string());
    }

    if let Some(start) = generation_start {
        let gen_secs = start.elapsed().as_secs();
        eprintln!("[BENCHMARK] generation_time={gen_secs}");
    }

    emit_gui_progress_update(99.0, "Finalizing world...");

    // For Bedrock format, emit event to open the mcworld file
    if world_format == WorldFormat::BedrockMcWorld {
        if let Some(path_str) = output_path.to_str() {
            emit_show_in_folder(path_str);
        }
    }

    Ok(output_path)
}

