use crate::args::Args;
use crate::coordinate_system::cartesian::{XZBBox, XZPoint};
use crate::coordinate_system::geographic::{LLBBox, LLPoint};
use crate::coordinate_system::transformation::CoordTransformer;
use crate::data_processing::{self, GenerationOptions};
use crate::ground::{self, Ground};
use crate::map_transformation;
use crate::osm_parser;
use crate::overture;
use crate::progress::{self, emit_gui_progress_update};
use crate::retrieve_data;
use crate::telemetry::{self, send_log, LogLevel};
use crate::version_check;
use crate::world_editor::WorldFormat;
use colored::Colorize;
use log::LevelFilter;
use rfd::FileDialog;
use std::path::{Path, PathBuf};
use std::{env, fs};
use tauri_plugin_log::{Builder as LogBuilder, Target, TargetKind};

pub fn run_gui() {
    // Configure thread pool with 90% CPU cap to keep system responsive
    crate::floodfill_cache::configure_rayon_thread_pool(0.9);

    // Clean up old cached elevation tiles on startup
    crate::elevation_data::cleanup_old_cached_tiles();

    // Launch the UI
    println!("Launching UI...");

    // Install panic hook for crash reporting
    telemetry::install_panic_hook();

    // Workaround WebKit2GTK issue with NVIDIA drivers and graphics issues
    // Source: https://github.com/tauri-apps/tauri/issues/10702
    #[cfg(target_os = "linux")]
    unsafe {
        // Disable problematic GPU features that cause map loading issues
        env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");

        // Force software rendering for better compatibility
        env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        env::set_var("GALLIUM_DRIVER", "softpipe");

        // Note: Removed sandbox disabling for security reasons
        // Note: Removed Qt WebEngine flags as they don't apply to Tauri
    }

    tauri::Builder::default()
        .plugin(
            LogBuilder::default()
                .level(LevelFilter::Info)
                .targets([
                    Target::new(TargetKind::LogDir {
                        file_name: Some("arnis".into()),
                    }),
                    Target::new(TargetKind::Stdout),
                ])
                .build(),
        )
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            gui_get_default_save_path,
            gui_set_save_path,
            gui_pick_save_directory,
            gui_start_generation,
            gui_get_version,
            gui_check_for_updates,
            gui_show_in_folder
        ])
        .setup(|app| {
            let app_handle = app.handle();
            let main_window = tauri::Manager::get_webview_window(app_handle, "main")
                .expect("Failed to get main window");
            progress::set_main_window(main_window);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Error while starting the application UI (Tauri)");
}

/// Detects the default Minecraft Java Edition saves directory for the current OS.
/// Checks standard install paths including Flatpak on Linux.
/// Falls back to Desktop, then current directory.
fn detect_minecraft_saves_directory() -> PathBuf {
    // Try standard Minecraft saves directories per OS
    let mc_saves: Option<PathBuf> = if cfg!(target_os = "windows") {
        env::var("APPDATA")
            .ok()
            .map(|appdata| PathBuf::from(appdata).join(".minecraft").join("saves"))
    } else if cfg!(target_os = "macos") {
        dirs::home_dir().map(|home| {
            home.join("Library/Application Support/minecraft")
                .join("saves")
        })
    } else if cfg!(target_os = "linux") {
        dirs::home_dir().map(|home| {
            let flatpak_path = home.join(".var/app/com.mojang.Minecraft/.minecraft/saves");
            if flatpak_path.exists() {
                flatpak_path
            } else {
                home.join(".minecraft/saves")
            }
        })
    } else {
        None
    };

    if let Some(saves_dir) = mc_saves {
        if saves_dir.exists() {
            return saves_dir;
        }
    }

    // Fallback to Desktop
    if let Some(desktop) = dirs::desktop_dir() {
        if desktop.exists() {
            return desktop;
        }
    }

    // Last resort: current directory
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Returns the default save path (auto-detected on first run).
/// The frontend stores/retrieves this via localStorage and passes it here for validation.
#[tauri::command]
fn gui_get_default_save_path() -> String {
    detect_minecraft_saves_directory().display().to_string()
}

/// Validates and returns a user-provided save path.
/// Returns the path string if valid, or an error message.
#[tauri::command]
fn gui_set_save_path(path: String) -> Result<String, String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err("Path does not exist.".to_string());
    }
    if !p.is_dir() {
        return Err("Path is not a directory.".to_string());
    }
    Ok(path)
}

/// Opens a native folder-picker dialog and returns the chosen path.
#[tauri::command]
fn gui_pick_save_directory(start_path: String) -> Result<String, String> {
    let start = PathBuf::from(&start_path);
    let mut dialog = FileDialog::new();
    if start.is_dir() {
        dialog = dialog.set_directory(&start);
    }
    match dialog.pick_folder() {
        Some(folder) => Ok(folder.display().to_string()),
        None => Ok(start_path),
    }
}

#[tauri::command]
fn gui_get_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
fn gui_check_for_updates() -> Result<bool, String> {
    match version_check::check_for_updates() {
        Ok(is_newer) => Ok(is_newer),
        Err(e) => Err(format!("Error checking for updates: {e}")),
    }
}


/// Reveals a file or folder in the system file explorer.
/// On Windows, tries to open files with the default application first (e.g. .mcworld with
/// Minecraft Bedrock), falling back to Explorer. Directories always open in Explorer.
#[tauri::command]
fn gui_show_in_folder(path: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // On Windows, try to open with default application (e.g. .mcworld with Minecraft Bedrock)
        // For directories, `start ""` opens Explorer directly. Falls back to explorer /select.
        if std::process::Command::new("cmd")
            .args(["/C", "start", "", &path])
            .spawn()
            .is_err()
        {
            std::process::Command::new("explorer")
                .args(["/select,", &path])
                .spawn()
                .map_err(|e| format!("Failed to open explorer: {}", e))?;
        }
    }

    #[cfg(target_os = "macos")]
    {
        // On macOS, just reveal in Finder
        std::process::Command::new("open")
            .args(["-R", &path])
            .spawn()
            .map_err(|e| format!("Failed to open Finder: {}", e))?;
    }

    #[cfg(target_os = "linux")]
    {
        // On Linux, just show in file manager
        let path_parent = std::path::Path::new(&path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());

        // Try nautilus with select first, then fall back to xdg-open on parent
        if std::process::Command::new("nautilus")
            .args(["--select", &path])
            .spawn()
            .is_err()
        {
            let _ = std::process::Command::new("xdg-open")
                .arg(&path_parent)
                .spawn();
        }
    }

    Ok(())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
#[allow(unused_variables)]
fn gui_start_generation(
    bbox_text: String,
    selected_world: String,
    world_scale: f64,
    ground_level: i32,
    terrain_enabled: bool,
    skip_osm_objects: bool,
    interior_enabled: bool,
    roof_enabled: bool,
    fillground_enabled: bool,
    land_cover_enabled: bool, // renamed from city_boundaries_enabled
    disable_height_limit: bool,
    spawn_point: Option<(f64, f64)>,
    telemetry_consent: bool,
    rotation_angle: f64,
    custom_osm_data: Option<String>,
) -> Result<(), String> {
    use progress::emit_gui_error;
    use LLBBox;

    // Store telemetry consent for crash reporting
    telemetry::set_telemetry_consent(telemetry_consent);

    // Send generation click telemetry
    telemetry::send_generation_click();

    tauri::async_runtime::spawn(async move {
        if let Err(e) = tokio::task::spawn_blocking(move || {
            let world_format = WorldFormat::BedrockMcWorld;

            // Check available disk space before starting generation (minimum 3GB required)
            const MIN_DISK_SPACE_BYTES: u64 = 3 * 1024 * 1024 * 1024; // 3 GB
            let check_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            
            match fs2::available_space(&check_path) {
                Ok(available) if available < MIN_DISK_SPACE_BYTES => {
                    let error_msg = "Not enough disk space available.".to_string();
                    eprintln!("{error_msg}");
                    emit_gui_error(&error_msg);
                    return Err(error_msg);
                }
                Err(e) => {
                    // Log warning but don't block generation if we can't check space
                    eprintln!("Warning: Could not check disk space: {e}");
                }
                _ => {} // Sufficient space available
            }

            // Parse the bounding box from the text with proper error handling
            let bbox = match LLBBox::from_str(&bbox_text) {
                Ok(bbox) => bbox,
                Err(e) => {
                    let error_msg = format!("Failed to parse bounding box: {e}");
                    eprintln!("{error_msg}");
                    emit_gui_error(&error_msg);
                    return Err(error_msg);
                }
            };

            // Determine output path and level name based on format
            let (generation_path, level_name) = {
                // Bedrock: generate .mcworld on Desktop with location-based name
                let output_dir = crate::world_utils::get_bedrock_output_directory();
                let (output_path, lvl_name) =
                    crate::world_utils::build_bedrock_output(&bbox, output_dir);
                (output_path, Some(lvl_name))
            };

            // Calculate MC spawn coordinates from lat/lng if spawn point was provided
            // Otherwise, default to X=1, Z=1 (relative to xzbbox min coordinates)
            let mc_spawn_point: Option<(i32, i32)> = if let Ok((transformer, pre_rot_bbox)) =
                CoordTransformer::llbbox_to_xzbbox(&bbox, world_scale)
            {
                let (sx, sz) = if let Some((lat, lng)) = spawn_point {
                    if let Ok(llpoint) = LLPoint::new(lat, lng) {
                        let xzpoint = transformer.transform_point(llpoint);
                        (xzpoint.x, xzpoint.z)
                    } else {
                        calculate_default_spawn(&pre_rot_bbox)
                    }
                } else {
                    calculate_default_spawn(&pre_rot_bbox)
                };
                Some(map_transformation::rotate::rotate_xz_point(
                    sx,
                    sz,
                    rotation_angle.clamp(-90.0, 90.0),
                    &pre_rot_bbox,
                ))
            } else {
                None
            };

            // Create generation options
            let generation_options = GenerationOptions {
                path: generation_path.clone(),
                format: world_format,
                level_name,
                spawn_point: mc_spawn_point,
            };

            // Create an Args instance with the chosen bounding box
            let args: Args = Args {
                bbox,
                file: None,
                save_json_file: None,
                path: Some(generation_path.clone()),
                downloader: "requests".to_string(),
                scale: world_scale,
                ground_level,
                terrain: terrain_enabled,
                interior: interior_enabled,
                roof: roof_enabled,
                fillground: fillground_enabled,
                land_cover: land_cover_enabled,
                debug: false,
                timeout: Some(std::time::Duration::from_secs(40)),
                spawn_lat: None,
                spawn_lng: None,
                rotation: rotation_angle.clamp(-90.0, 90.0),
                disable_height_limit,
                benchmark: false,
            };

            // If skip_osm_objects is true (terrain-only mode), skip fetching and processing OSM data
            if skip_osm_objects {
                // Generate ground data (terrain) for terrain-only mode
                let ground = ground::generate_ground_data(&args);

                // Create empty parsed_elements and xzbbox for terrain-only mode
                let parsed_elements = Vec::new();
                let (_coord_transformer, xzbbox) =
                    CoordTransformer::llbbox_to_xzbbox(&args.bbox, args.scale)
                        .map_err(|e| format!("Failed to create coordinate transformer: {}", e))?;

                let _ = data_processing::generate_world_with_options(
                    parsed_elements,
                    xzbbox.clone(),
                    args.bbox,
                    ground,
                    &args,
                    generation_options.clone(),
                );
                
                emit_gui_progress_update(100.0, "Done! World generation completed.");
                println!("{}", "Done! World generation completed.".green().bold());

                return Ok(());
            }

            // Pass custom OsmData directly if provided by the Canvas, otherwise fetch from Overpass
            let raw_data_result = if let Some(osm_json) = custom_osm_data {
                serde_json::from_str::<crate::osm_parser::OsmData>(&osm_json)
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
            } else {
                retrieve_data::fetch_data_from_overpass(args.bbox, args.debug, "requests", None)
            };

            // Run data fetch and world generation (standard mode: objects + terrain, or objects only)
            match raw_data_result {
                Ok(raw_data) => {
                    let (mut parsed_elements, mut xzbbox) =
                        osm_parser::parse_osm_data(raw_data, args.bbox, args.scale, args.debug);

                    // Fetch supplementary building data from Overture Maps
                    {
                        let overture_elements =
                            overture::fetch_overture_buildings(&args.bbox, args.scale, args.debug);
                        if !overture_elements.is_empty() {
                            let unique_overture = overture::deduplicate_against_osm(
                                overture_elements,
                                &parsed_elements,
                            );
                            parsed_elements.extend(unique_overture);
                        }
                    }

                    parsed_elements.sort_by(|el1, el2| {
                        let (el1_priority, el2_priority) =
                            (osm_parser::get_priority(el1), osm_parser::get_priority(el2));
                        match (
                            el1.tags().contains_key("landuse"),
                            el2.tags().contains_key("landuse"),
                        ) {
                            (true, false) => std::cmp::Ordering::Greater,
                            (false, true) => std::cmp::Ordering::Less,
                            _ => el1_priority.cmp(&el2_priority),
                        }
                    });

                    let mut ground = ground::generate_ground_data(&args);

                    // Transform map (parsed_elements). Operations are defined in a json file
                    map_transformation::transform_map(
                        &mut parsed_elements,
                        &mut xzbbox,
                        &mut ground,
                    );

                    // Apply rotation if specified
                    if rotation_angle.abs() > f64::EPSILON {
                        map_transformation::rotate::rotate_world(
                            rotation_angle.clamp(-90.0, 90.0),
                            &mut parsed_elements,
                            &mut xzbbox,
                            &mut ground,
                        )
                        .map_err(|e| format!("Rotation failed: {e}"))?;
                    }

                    let _ = data_processing::generate_world_with_options(
                        parsed_elements,
                        xzbbox.clone(),
                        args.bbox,
                        ground,
                        &args,
                        generation_options.clone(),
                    );
                    
                    emit_gui_progress_update(100.0, "Done! World generation completed.");
                    println!("{}", "Done! World generation completed.".green().bold());

                    Ok(())
                }
                Err(e) => {
                    emit_gui_error(&e.to_string());
                    // Session lock will be automatically released when _session_lock goes out of scope
                    Err(e.to_string())
                }
            }
        })
        .await
        {
            let error_msg = format!("Error in blocking task: {e}");
            eprintln!("{error_msg}");
            emit_gui_error(&error_msg);
        }
    });

    Ok(())
}
