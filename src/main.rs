use clap::{Parser, Subcommand};
use std::fs;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH, Duration};
use std::process::{self, Command};
use sha2::{Sha256, Digest};
use nix::sys::statvfs;
use std::os::unix::process::CommandExt;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use procfs::process::Process;
use fs2::FileExt;
use ignore::WalkBuilder;
use std::io::{self, Write};

#[derive(Parser, Debug)]
#[command(version, about = "A tool to run and archive projects.")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run and archive a script or project
    Run {
        /// The program to execute (e.g., "python3")
        command: String,

        /// The file to archive upon success
        file: String,

        /// Remaining arguments to pass to the script
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        script_args: Vec<String>,

        /// Require exit code 0 for success (default is relaxed: exit code 0 or empty stderr)
        #[arg(long)]
        strict: bool,

        /// Optional folder to monitor for changes and archive
        #[arg(long)]
        watch: Option<PathBuf>,
    },
    /// List all tracked projects
    List,
    /// Remove a project archive
    Rm {
        /// The project name to remove
        project: String,
    },
    /// Prune old backups for a project
    Prune {
        /// The project name to prune
        project: String,
        /// Maximum number of backups to keep (default: 100)
        #[arg(long, default_value_t = 100)]
        max_backups: usize,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run { command, file, script_args, strict, watch } => {
            run_project(command, file, script_args, strict, watch);
        }
        Commands::List => list_projects(),
        Commands::Rm { project } => remove_project(&project),
        Commands::Prune { project, max_backups } => prune_project(&project, max_backups),
    }
}

fn run_project(cmd: String, file: String, script_args: Vec<String>, strict: bool, watch: Option<PathBuf>) {
    let file_path = Path::new(&file);
    if !file_path.exists() { eprintln!("Error: File '{}' does not exist.", file); process::exit(1); }
    if fs::metadata(file_path).map(|m| !m.is_file()).unwrap_or(true) { eprintln!("Error: Path '{}' is not a file.", file); process::exit(1); }
    if fs::File::open(file_path).is_err() { eprintln!("Error: File '{}' is not readable.", file); process::exit(1); }
    if fs::metadata(file_path).map(|m| m.len() == 0).unwrap_or(true) { eprintln!("Error: File '{}' is empty. Skipping execution.", file); process::exit(1); }

    let lock_path = format!("{}.lock", file);
    let lock_file = fs::File::create(&lock_path).expect("Failed to create lock file");
    if lock_file.try_lock_exclusive().is_err() { eprintln!("Error: Another instance is already testing file '{}'.", file); process::exit(1); }
    if !validate_script(file_path) { eprintln!("Error: Script '{}' failed syntax validation.", file); let _ = fs::remove_file(&lock_path); process::exit(1); }

    let temp_dir = env::temp_dir();
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let temp_script_path = temp_dir.join(format!("va_exec_{}_{}", timestamp, file_path.file_name().unwrap().to_str().unwrap()));
    let _snapshot_path = temp_dir.join(format!("va_snapshot_{}_{}", timestamp, file_path.file_name().unwrap().to_str().unwrap()));

    if fs::copy(file_path, &temp_script_path).is_err() || fs::copy(file_path, &_snapshot_path).is_err() {
        eprintln!("Error creating temporary execution copies.");
        let _ = fs::remove_file(&lock_path);
        process::exit(1);
    }

    let child_pid = Arc::new(Mutex::new(None::<Pid>));
    let interrupted = Arc::new(AtomicBool::new(false));
    let int_ref = interrupted.clone();
    let cp = child_pid.clone();

    ctrlc::set_handler(move || {
        int_ref.store(true, Ordering::SeqCst);
        if let Ok(pid_guard) = cp.lock() {
            if let Some(pid) = *pid_guard {
                let _ = signal::kill(Pid::from_raw(-pid.as_raw()), Signal::SIGTERM);
            }
        }
    }).expect("Error setting Ctrl-C handler");

    println!("Running program: {}", cmd);
    let mut process = Command::new(&cmd);
    process.arg(&temp_script_path);
    process.stdout(std::process::Stdio::piped());
    process.stderr(std::process::Stdio::piped());
    unsafe { process.pre_exec(|| { nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0)).map(|_| ()).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)) }); }
    for arg in &script_args { process.arg(arg); }

    let mut child = process.spawn().expect("Failed to start process");
    if let Ok(mut pid_guard) = child_pid.lock() { *pid_guard = Some(Pid::from_raw(child.id() as i32)); }

    // --- Monitoring Thread ---
    let mon_cp = child_pid.clone();
    thread::spawn(move || {
        let mut last_activity = SystemTime::now();
        let mut last_cpu_time = 0;
        let mut warned = false;
        loop {
            thread::sleep(Duration::from_secs(5));
            let pid = if let Ok(pid_guard) = mon_cp.lock() { *pid_guard } else { None };
            if let Some(pid) = pid {
                if let Ok(proc) = Process::new(pid.as_raw()) {
                    if let Ok(stat) = proc.stat() {
                        let current_cpu_time = stat.utime + stat.stime;
                        if current_cpu_time != last_cpu_time {
                            last_activity = SystemTime::now();
                            last_cpu_time = current_cpu_time;
                            warned = false;
                        }
                    }
                } else { break; }
            } else { thread::sleep(Duration::from_secs(1)); continue; }

            let idle_duration = SystemTime::now().duration_since(last_activity).unwrap().as_secs();
            if idle_duration > 60 && !warned {
                if let Ok(pid_guard) = mon_cp.lock() {
                    if let Some(pid) = *pid_guard { println!("\n[WARN] Process {} has been idle (no CPU usage) for {}s. Kill? [y/N]", pid.as_raw(), idle_duration); }
                }
                warned = true;
            }
            if idle_duration > 1800 {
                if let Ok(mut child) = Command::new("mail").args(["-s", "Archiver Alert: Stalled Process", "lou@samsung"]).stdin(std::process::Stdio::piped()).spawn() {
                    use std::io::Write;
                    if let Some(mut stdin) = child.stdin.take() { let _ = write!(stdin, "Process has been idle for {} seconds", idle_duration); }
                }
                warned = false; last_activity = SystemTime::now();
            }
        }
    });

    let output = child.wait_with_output().expect("Failed to wait on child");
    let _ = fs::remove_file(&temp_script_path);
    let _ = fs::remove_file(&_snapshot_path);
    let _ = fs::remove_file(&lock_path);

    let mut success = output.status.success() || (!strict && output.stderr.is_empty());
    
    if interrupted.load(Ordering::SeqCst) {
        print!("\nExecution interrupted. Archive results? [y/N]: ");
        io::stdout().flush().unwrap();
        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();
        if input.trim().to_lowercase() == "y" { success = true; }
    }

    if success {
        println!("✔ SUCCESS (exit code {:?})", output.status.code());
        let files_to_archive = if let Some(watch_dir) = &watch { get_monitored_files(watch_dir) } else { vec![file_path.to_path_buf()] };
        for path in files_to_archive {
            if let Err(e) = archive_file(&path, watch.as_deref().unwrap_or(Path::new("single_file"))) {
                eprintln!("Error archiving file {:?}: {}", path, e);
            }
        }
    } else {
        eprintln!("✖ FAILURE (exit code: {:?})", output.status.code());
        if !output.stderr.is_empty() { eprintln!("Error output:\n{}", String::from_utf8_lossy(&output.stderr)); }
        process::exit(1);
    }
}

fn validate_script(path: &Path) -> bool {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    match ext {
        "py" => Command::new("python3").args(["-m", "py_compile", path.to_str().unwrap()]).status().map(|s| s.success()).unwrap_or(false),
        "sh" | "bash" => Command::new("bash").args(["-n", path.to_str().unwrap()]).status().map(|s| s.success()).unwrap_or(false),
        "rs" => Command::new("rustc").args(["--check", path.to_str().unwrap()]).status().map(|s| s.success()).unwrap_or(false),
        _ => true,
    }
}

fn get_monitored_files(root: &Path) -> Vec<PathBuf> {
    WalkBuilder::new(root)
        .add_custom_ignore_filename(".vaignore")
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != "__pycache__" && name != "target" && !name.ends_with(".md")
        })
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map_or(false, |ft| ft.is_file()))
        .map(|e| e.path().to_path_buf())
        .collect()
}

fn archive_file(file_path: &Path, project_root: &Path) -> std::io::Result<()> {
    let home = env::var("HOME").expect("HOME not set");
    let archive_root = format!("{}/.validation_archiver", home);
    let project_name = project_root.file_name().unwrap().to_str().unwrap();
    let relative_path = file_path.strip_prefix(project_root).unwrap_or(file_path);
    let file_name = file_path.file_name().unwrap().to_str().unwrap();
    let archive_path = Path::new(&archive_root).join(project_name).join(relative_path);

    let latest_hash = get_latest_hash(&archive_path, file_name);
    let current_hash = compute_file_hash(file_path)?;
    if let Some(hash) = latest_hash { if hash == current_hash { return Ok(()); } }

    let stats = statvfs::statvfs(archive_root.as_str()).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let available_space = stats.blocks_available() * stats.fragment_size();
    let file_size = fs::metadata(file_path)?.len();
    if available_space < file_size { return Err(std::io::Error::new(std::io::ErrorKind::OutOfMemory, "Insufficient disk space")); }

    rotate_backups(&archive_path, 100)?;
    let start = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let archive_dir = archive_path.join(start.to_string());
    fs::create_dir_all(&archive_dir)?;
    let destination = archive_dir.join(file_name);
    let temp_dest = archive_dir.join(format!(".tmp_{}", file_name));
    fs::copy(file_path, &temp_dest)?;
    fs::rename(&temp_dest, &destination)?;
    fs::write(archive_dir.join(".hash"), current_hash)?;
    println!("📦 Archived to {}", destination.display());
    Ok(())
}

fn rotate_backups(archive_path: &Path, max_backups: usize) -> std::io::Result<()> {
    if !archive_path.exists() { return Ok(()); }
    let mut entries: Vec<_> = fs::read_dir(archive_path)?.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).collect();
    if entries.len() > max_backups {
        entries.sort_by_key(|e| e.metadata().unwrap().modified().unwrap());
        for i in 0..(entries.len() - max_backups) { fs::remove_dir_all(entries[i].path())?; }
    }
    Ok(())
}

fn compute_file_hash(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn get_latest_hash(archive_path: &Path, _file_name: &str) -> Option<String> {
    if !archive_path.exists() { return None; }
    let mut entries: Vec<_> = fs::read_dir(archive_path).ok()?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.metadata().ok().map(|m| m.modified().ok()).flatten());
    let latest_dir = entries.last()?.path();
    fs::read_to_string(latest_dir.join(".hash")).ok()
}

fn list_projects() {
    let home = env::var("HOME").expect("HOME not set");
    let archive_root = format!("{}/.validation_archiver", home);
    let root_path = Path::new(&archive_root);
    if !root_path.exists() { println!("No projects tracked yet."); return; }
    println!("Tracked projects and files:");
    if let Ok(entries) = fs::read_dir(root_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name().into_string().unwrap_or_else(|_| "unknown".to_string());
                if name == "single_file" { println!(" - [Single File Archive]"); } else { println!(" - [Project] {}", name); }
            }
        }
    }
}

fn remove_project(project: &str) {
    let home = env::var("HOME").expect("HOME not set");
    let archive_root = format!("{}/.validation_archiver", home);
    let project_path = Path::new(&archive_root).join(project);
    if !project_path.exists() { eprintln!("Error: Project '{}' not found.", project); return; }
    println!("Are you sure you want to remove project '{}'? [y/N]", project);
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    if input.trim().to_lowercase() == "y" {
        fs::remove_dir_all(&project_path).expect("Failed to remove project directory");
        println!("Project '{}' removed.", project);
    } else { println!("Removal cancelled."); }
}

fn prune_project(project: &str, max_backups: usize) {
    let home = env::var("HOME").expect("HOME not set");
    let archive_root = format!("{}/.validation_archiver", home);
    let project_path = Path::new(&archive_root).join(project);
    if !project_path.exists() { eprintln!("Error: Project '{}' not found.", project); return; }
    fn walk_and_prune(path: &Path, max: usize) -> std::io::Result<()> {
        if path.is_dir() {
            let entries: Vec<_> = fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
            let is_file_archive = entries.iter().all(|e| e.file_name().to_string_lossy().parse::<u64>().is_ok());
            if is_file_archive { rotate_backups(path, max)?; } else { for entry in entries { walk_and_prune(&entry.path(), max)?; } }
        }
        Ok(())
    }
    if let Err(e) = walk_and_prune(&project_path, max_backups) { eprintln!("Error pruning project: {}", e); }
    else { println!("Project '{}' pruned to {} backups.", project, max_backups); }
}
