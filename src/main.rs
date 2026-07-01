use clap::Parser;
use std::process::Command;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::fs;
use std::process;
use sha2::{Sha256, Digest};
use nix::sys::statvfs;

#[derive(Parser, Debug)]
#[command(version, about = "A tool to run a command with a file and archive the file on success.")]
struct Args {
    /// The program to execute (e.g., "python")
    command: String,

    /// The file to archive upon success
    file: String,

    /// Remaining arguments to pass to the script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    script_args: Vec<String>,
}

fn main() {
    let args = Args::parse();

    // --- Pre-run validation ---
    let file_path = Path::new(&args.file);
    if !file_path.exists() {
        eprintln!("Error: File '{}' does not exist.", args.file);
        process::exit(1);
    }
    if fs::metadata(file_path).map(|m| !m.is_file()).unwrap_or(true) {
        eprintln!("Error: Path '{}' is not a file.", args.file);
        process::exit(1);
    }
    if fs::File::open(file_path).is_err() {
        eprintln!("Error: File '{}' is not readable.", args.file);
        process::exit(1);
    }

    // Check if empty
    if fs::metadata(file_path).map(|m| m.len() == 0).unwrap_or(true) {
        eprintln!("Error: File '{}' is empty. Skipping execution.", args.file);
        process::exit(1);
    }

    // --- Create isolated temporary copy for execution ---
    let temp_dir = env::temp_dir();
    let temp_script_path = temp_dir.join(format!(
        "validation_archiver_{}_{}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
        file_path.file_name().unwrap().to_str().unwrap()
    ));

    if let Err(e) = fs::copy(file_path, &temp_script_path) {
        eprintln!("Error creating temporary execution copy: {}", e);
        process::exit(1);
    }

    println!("Running program: {}", args.command);
    println!("Archivable file: {}", args.file);
    println!("Program args: {:?}", args.script_args);

    let mut process = Command::new(&args.command);
    process.arg(&temp_script_path);

    for arg in &args.script_args {
        process.arg(arg);
    }

    // Inherit stderr to show script errors to the user
    let result = process.status();

    // Cleanup temp script
    let _ = fs::remove_file(&temp_script_path);

    match result {
        Ok(status) => {
            if status.success() {
                println!("✔ SUCCESS (exit code 0)");
                if let Err(e) = archive_file(&args.file) {
                    eprintln!("Error archiving file: {}", e);
                    process::exit(1);
                }
            } else {
                eprintln!("✖ FAILURE (exit code: {:?})", status.code());
                process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Execution failed: {}", e);
            process::exit(1);
        }
    }
}

fn archive_file(file_path: &str) -> std::io::Result<()> {
    let home = env::var("HOME").expect("HOME not set");
    let archive_root = format!("{}/.validation_archiver", home);

    let path = Path::new(file_path);
    let file_name = path.file_name().unwrap().to_str().unwrap();

    // --- Incremental Backup Check ---
    let latest_hash = get_latest_hash(&archive_root, file_name);
    let current_hash = compute_file_hash(path)?;

    if let Some(hash) = latest_hash {
        if hash == current_hash {
            println!("ℹ No changes detected, skipping archive.");
            return Ok(());
        }
    }

    // --- Disk Space Check ---
    let stats = statvfs::statvfs(archive_root.as_str()).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let available_space = stats.blocks_available() * stats.fragment_size();
    let file_size = fs::metadata(file_path)?.len();

    if available_space < file_size {
        return Err(std::io::Error::new(std::io::ErrorKind::OutOfMemory, "Insufficient disk space"));
    }

    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Structure: <archive_root>/<file_name>/<timestamp>/
    let archive_dir = PathBuf::from(format!("{}/{}/{}", archive_root, file_name, start));
    fs::create_dir_all(&archive_dir)?;

    let destination = archive_dir.join(file_name);
    let temp_dest = archive_dir.join(format!(".tmp_{}", file_name));

    // Atomic Save: Copy to temp then rename
    fs::copy(file_path, &temp_dest)?;
    fs::rename(&temp_dest, &destination)?;

    // Store hash for incremental check
    fs::write(archive_dir.join(".hash"), current_hash)?;

    println!("📦 Archived to {}", destination.display());
    Ok(())
}

fn compute_file_hash(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn get_latest_hash(archive_root: &str, file_name: &str) -> Option<String> {
    let file_dir = Path::new(archive_root).join(file_name);
    if !file_dir.exists() { return None; }

    // Find latest timestamp folder
    let mut entries: Vec<_> = fs::read_dir(file_dir).ok()?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.metadata().ok().map(|m| m.modified().ok()).flatten());
    let latest_dir = entries.last()?.path();

    fs::read_to_string(latest_dir.join(".hash")).ok()
}
