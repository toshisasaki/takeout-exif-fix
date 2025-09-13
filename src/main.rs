use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use clap::Parser;
use walkdir::WalkDir;
use serde_json::Value;
use exif::{In, Tag};
use filetime::{FileTime, set_file_times};
use chrono::{NaiveDateTime, Datelike, DateTime, TimeZone, Utc};
use pretty_env_logger;
use log::*;
use rayon::prelude::*;
use std::sync::{Mutex, MutexGuard, Arc, OnceLock};
use std::collections::HashSet;


// A mutex to manage reserved file paths during parallel processing
pub static MUTEX: OnceLock<Arc<Mutex<HashSet<String>>>> = OnceLock::new();

/// A tool to organize photos based on their metadata
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// The input directory containing photos and metadata files
    #[arg(short, long)]
    input: String,

    /// The output directory where organized photos will be stored
    #[arg(short, long)]
    output: String,
}

fn main() {
    pretty_env_logger::init();

    let args = Cli::parse();

    let input_directory = &args.input;
    let output_directory = &args.output;

    if !Path::new(input_directory).exists() {
        error!("Input directory does not exist: {}", input_directory);
        std::process::exit(1);
    }

    if !Path::new(output_directory).exists() {
        error!("Output directory does not exist: {}", output_directory);
        std::process::exit(1);
    }

    log::info!("Starting the photo organizer...");

    let metadata_map = parse_metadata_files(input_directory);
    process_directory_parallel(input_directory, output_directory, &metadata_map);
}

/// Parse all metadata files and store relevant information in a HashMap
fn parse_metadata_files(directory: &str) -> HashMap<String, chrono::DateTime<Utc>> {
    let metadata_map = std::sync::Mutex::new(HashMap::new());

    WalkDir::new(directory)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
        .par_bridge() // Parallelize the iterator
        .for_each(|entry| {
            let path = entry.path();
            if let Ok(mut file) = File::open(path) {
                let mut contents = String::new();
                if file.read_to_string(&mut contents).is_ok() {
                    if let Ok(metadata) = serde_json::from_str::<Value>(&contents) {
                        if let Some(photo_filename) = metadata["title"].as_str() {
                            if let Some(photo_taken_timestamp) = metadata["photoTakenTime"]["timestamp"].as_str() {
                                if let Ok(timestamp) = photo_taken_timestamp.parse::<i64>() {
                                    if let Some(parsed_time) = DateTime::from_timestamp(timestamp, 0) {
                                        let mut metadata_map = metadata_map.lock().unwrap();
                                        metadata_map.insert(photo_filename.to_string(), parsed_time);
                                    } else {
                                        error!("Failed to parse timestamp for file: {}", photo_filename);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

    std::sync::Mutex::into_inner(metadata_map).unwrap()
}

/// Process the directory and organize photos based on metadata or EXIF data
fn process_directory_parallel(directory: &str, output_directory: &str, metadata_map: &HashMap<String, chrono::DateTime<Utc>>) {
    WalkDir::new(directory)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) != Some("json"))
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) != Some("zip"))
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) != Some("html"))
        .par_bridge() // Parallelize the iterator
        .for_each(|entry| {
            let path = entry.path();
            if let Some(filename) = path.file_name().and_then(|name| name.to_str()) {
                if let Some(&parsed_time) = metadata_map.get(filename) {
                    info!("Processing photo file {:?} using metadata timestamp: {}", path, parsed_time);
                    // Process the photo using metadata
                    if let Err(e) = organize_and_update_file(path, parsed_time, output_directory) {
                        error!("Error processing photo file {:?}: {}", path, e);
                    }
                } else {
                    // Process the photo using EXIF data
                    info!("Processing photo file {:?} using EXIF data", path);
                    if let Err(e) = process_photo_file(path, output_directory) {
                        error!("Error processing photo file {:?}: {}", path, e);
                    }
                }
            }
        });
}

/// Process a photo file using EXIF metadata
fn process_photo_file(photo_path: &Path, output_directory: &str) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(photo_path)?;
    let mut bufreader = std::io::BufReader::new(file);

    if let Ok(exif) = exif::Reader::new().read_from_container(&mut bufreader) {
        if let Some(field) = exif.get_field(Tag::DateTimeOriginal, In::PRIMARY) {
            info!("Found EXIF DateTimeOriginal field in {:?}", photo_path);
            let date_time_original = field.display_value().to_string();
            debug!("EXIF DateTimeOriginal: {}", date_time_original);
            if let Ok(parsed_time) = NaiveDateTime::parse_from_str(&date_time_original, "%Y-%m-%d %H:%M:%S") {
                // Convert to UTC
                let parsed_time_utc = Utc.from_local_datetime(&parsed_time).unwrap();
                organize_and_update_file(photo_path, parsed_time_utc, output_directory)?;
            } else {
                warn!("Failed to parse EXIF DateTimeOriginal for file: {:?}", photo_path);
                process_photo_file_with_creation_time(photo_path, output_directory)?;
            }
        } else {
            warn!("No EXIF DateTimeOriginal field found in {:?}", photo_path);
            process_photo_file_with_creation_time(photo_path, output_directory)?;
        }
    } else {
        warn!("No EXIF metadata found in {:?}", photo_path);
        process_photo_file_with_creation_time(photo_path, output_directory)?;
    }

    Ok(())
}

/// Fallback: Process a photo file using its creation timestamp if no metadata or EXIF data is available
fn process_photo_file_with_creation_time(photo_path: &Path, output_directory: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::metadata;
    let meta = metadata(photo_path)?;
    let created = meta.created().or_else(|_| meta.modified())?;
    let datetime: chrono::DateTime<Utc> = created.into();
    info!("Using file creation/modification time for {:?}", photo_path);
    organize_and_update_file(photo_path, datetime, output_directory)
}

/// A helper function to find a unique filename
/// This function appends a counter to the original filename. The counter is incremented until a unique filename is found.
fn find_unique_filename(base_dir: &Path, original_path: &Path, reserved_paths: &MutexGuard<HashSet<String>>) -> std::path::PathBuf {
    let mut counter = 1;
    loop {
        let file_stem = original_path.file_stem()
            .unwrap_or_else(|| std::ffi::OsStr::new("unnamed_file"));
        let extension = original_path.extension()
            .unwrap_or_else(|| std::ffi::OsStr::new(""));
        
        let new_file_name = if extension.is_empty() {
            format!("{}_{}", file_stem.to_string_lossy(), counter)
        } else {
            format!("{}_{}.{}", file_stem.to_string_lossy(), counter, extension.to_string_lossy())
        };

        // If the new file name is not in reserved paths and does not exist, return it
        if !reserved_paths.contains(base_dir.join(&new_file_name).to_string_lossy().as_ref()) && !base_dir.join(&new_file_name).exists() {
            return base_dir.join(new_file_name);
        }
        counter += 1;
    }
}

/// A function to get a unique filename to output the photo
/// This function ensures that no two threads write to the same file simultaneously
/// by using a mutex to lock the reserved paths during the check and insert operation.
/// First, it locks the reserved paths set, checks if the desired output path is already reserved or exists,
/// and if not, it reserves the path by inserting it into the set.
/// If the path is already reserved or exists, it tries again until a unique path is found.
/// Finally, it releases the lock before performing the file copy operation.
fn get_output_path(photo_path: &Path, target_dir: &Path) -> std::path::PathBuf {
     let mut reserved_paths = MUTEX
            .get_or_init(|| Arc::new(Mutex::new(HashSet::new())))
            .lock()
            .unwrap();
    let mut output_path = target_dir.join(photo_path.file_name().unwrap_or_else(|| std::ffi::OsStr::new("unnamed_file")));
    loop {
        if !reserved_paths.contains(output_path.to_string_lossy().as_ref()) && !output_path.exists() {
            reserved_paths.insert(output_path.to_string_lossy().to_string());
            break;
        }
        output_path = find_unique_filename(target_dir, photo_path, &reserved_paths);
    }
    output_path
}

/// Organize and update the file based on the parsed time
fn organize_and_update_file(photo_path: &Path, parsed_time: chrono::DateTime<Utc>, output_directory: &str) -> Result<(), Box<dyn std::error::Error>> {
    let year = parsed_time.year();
    let month = parsed_time.month();

    let month_name = match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "Unknown",
    };

    let year_dir = Path::new(output_directory).join(year.to_string());
    let month_dir = year_dir.join(month_name);
    fs::create_dir_all(&month_dir)?;

    let target_dir = if let Some(extension) = photo_path.extension().and_then(|ext| ext.to_str()) {
        month_dir.join(extension.to_lowercase())
    } else {
        month_dir.join("no_ext")
    };
    fs::create_dir_all(&target_dir)?;

    let output_path = get_output_path(photo_path, &target_dir);

    fs::copy(photo_path, &output_path)?;

    let unix_timestamp = parsed_time.timestamp();
    let file_time = FileTime::from_unix_time(unix_timestamp, 0);
    set_file_times(&output_path, file_time, file_time)?;

    Ok(())
}