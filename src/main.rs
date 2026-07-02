use clap::Parser;
use std::process::Command;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH, Duration};
use std::fs;
use std::process;
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

#[derive(Parser, Debug)]
#[command(version, about = "A tool to run a command with a file and archive the file on success.")]
struct Args {
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

    if fs::metadata(file_path).map(|m| m.len() == 0).unwrap_or(true) {
        eprintln!("Error: File '{}' is empty. Skipping execution.", args.file);
        process::exit(1);
    }

    // --- Concurrency Protection ---
    let lock_path = format!("{}.lock", args.file);
    let lock_file = fs::File::create(&lock_path).expect("Failed to create lock file");
    if lock_file.try_lock_exclusive().is_err() {
        eprintln!("Error: Another instance is already testing file '{}'.", args.file);
        process::exit(1);
    }

    // --- Syntax Validation ---
    if !validate_script(file_path) {
        eprintln!("Error: Script '{}' failed syntax validation.", args.file);
        let _ = fs::remove_file(&lock_path);
        process::exit(1);
    }

    // --- Create isolated temporary copy for execution ---
    let temp_dir = env::temp_dir();
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    
    let temp_script_path = temp_dir.join(format!("va_exec_{}_{}", timestamp, file_path.file_name().unwrap().to_str().unwrap()));
    let _snapshot_path = temp_dir.join(format!("va_snapshot_{}_{}", timestamp, file_path.file_name().unwrap().to_str().unwrap()));

    if fs::copy(file_path, &temp_script_path).is_err() || fs::copy(file_path, &_snapshot_path).is_err() {
        eprintln!("Error creating temporary execution copies.");
        let _ = fs::remove_file(&lock_path);
        process::exit(1);
    }

    // --- Signal Handling & Monitoring ---
    let child_pid = Arc::new(Mutex::new(None::<Pid>));
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    let cp = child_pid.clone();

    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
        if let Ok(pid_guard) = cp.lock() {
            if let Some(pid) = *pid_guard {
                let _ = signal::kill(Pid::from_raw(-pid.as_raw()), Signal::SIGTERM);
            }
        }
    }).expect("Error setting Ctrl-C handler");

    println!("Running program: {}", args.command);
    println!("Archivable file: {}", args.file);
    println!("Program args: {:?}", args.script_args);

    let mut process = Command::new(&args.command);
    process.arg(&temp_script_path);
    process.stdout(std::process::Stdio::piped());
    process.stderr(std::process::Stdio::piped());
    // Put child in its own process group
    unsafe { process.pre_exec(|| { nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0)).map(|_| ()).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)) }); }

    for arg in &args.script_args {
        process.arg(arg);
    }

    let mut child = process.spawn().expect("Failed to start process");
    let c_pid = Pid::from_raw(child.id() as i32);
    if let Ok(mut pid_guard) = child_pid.lock() {
        *pid_guard = Some(c_pid);
    }

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
                } else {
                    // Child died
                    break;
                }
            } else {
                // Should not happen if pid is correctly set
                thread::sleep(Duration::from_secs(1));
                continue;
            }

            let idle_duration = SystemTime::now().duration_since(last_activity).unwrap().as_secs();
            
            // 60s warning, 1800s (30m) email
            if idle_duration > 60 && !warned {
                if let Ok(pid_guard) = mon_cp.lock() {
                    if let Some(pid) = *pid_guard {
                         println!("\n[WARN] Process {} has been idle (no CPU usage) for {}s. Kill? [y/N]", pid.as_raw(), idle_duration);
                    }
                }
                warned = true;
            }
            
            if idle_duration > 1800 {
                // Send email alert
                if let Ok(mut child) = Command::new("mail")
                    .args(["-s", "Archiver Alert: Stalled Process", "lou@samsung"])
                    .stdin(std::process::Stdio::piped())
                    .spawn() {
                        use std::io::Write;
                        if let Some(mut stdin) = child.stdin.take() {
                            let _ = write!(stdin, "Process has been idle for {} seconds", idle_duration);
                        }
                    }
                // Reset warned to avoid email spam
                warned = false;
                last_activity = SystemTime::now();
            }
        }
    });

    let output = child.wait_with_output().expect("Failed to wait on child");

    // Cleanup
    let _ = fs::remove_file(&temp_script_path);
    let _ = fs::remove_file(&_snapshot_path);
    let _ = fs::remove_file(&lock_path);

    let is_success = output.status.success() || (!args.strict && output.stderr.is_empty());

    if is_success {
        println!("✔ SUCCESS (exit code {:?})", output.status.code());
        
        // --- Archive Logic ---
        let files_to_archive = if let Some(watch_dir) = &args.watch {
            get_monitored_files(watch_dir)
        } else {
            vec![file_path.to_path_buf()]
        };

        for path in files_to_archive {
            if let Err(e) = archive_file(&path, args.watch.as_deref().unwrap_or(Path::new("single_file"))) {
                eprintln!("Error archiving file {:?}: {}", path, e);
            }
        }
    } else {
        eprintln!("✖ FAILURE (exit code: {:?})", output.status.code());
        if !output.stderr.is_empty() {
            eprintln!("Error output:\n{}", String::from_utf8_lossy(&output.stderr));
        }
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
            name != "__pycache__" && name != "target"
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

    // --- Incremental Backup Check ---
    let latest_hash = get_latest_hash(&archive_path, file_name);
    let current_hash = compute_file_hash(file_path)?;

    if let Some(hash) = latest_hash {
        if hash == current_hash {
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

    // --- Rotation Policy ---
    rotate_backups(&archive_path, 100)?;

    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Structure: <archive_root>/<project_name>/<relative_path>/<timestamp>/
    let archive_dir = archive_path.join(start.to_string());
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

fn rotate_backups(archive_path: &Path, max_backups: usize) -> std::io::Result<()> {
    if !archive_path.exists() { return Ok(()); }
    
    let mut entries: Vec<_> = fs::read_dir(archive_path)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();

    if entries.len() >= max_backups {
        entries.sort_by_key(|e| e.metadata().unwrap().modified().unwrap());
        for i in 0..(entries.len() - max_backups + 1) {
            fs::remove_dir_all(entries[i].path())?;
        }
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

    // Find latest timestamp folder
    let mut entries: Vec<_> = fs::read_dir(archive_path).ok()?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.metadata().ok().map(|m| m.modified().ok()).flatten());
    let latest_dir = entries.last()?.path();

    fs::read_to_string(latest_dir.join(".hash")).ok()
}
