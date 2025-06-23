use compiler::codegen::EmittedFile;
use compiler::global_state::Module;
use compiler::type_::ModuleId;
use compiler::{codegen, fs, infer};

use serde::Deserialize;
use std::path::{Path};
use std::process::Command;

#[derive(Deserialize, Debug)]
enum Input {
    InferExpr(String),
    InferPackage(String),
    ParseExpr(String),
    ParseFile(String),
}

#[derive(Debug, Clone)]
enum TargetPlatform {
    Linux,
    Windows,
    Darwin,
    LinuxArm64,
    WindowsArm64,
    DarwinArm64,
}

impl TargetPlatform {
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "linux" | "linux-amd64" => Ok(TargetPlatform::Linux),
            "windows" | "windows-amd64" => Ok(TargetPlatform::Windows),
            "darwin" | "macos" | "darwin-amd64" => Ok(TargetPlatform::Darwin),
            "linux-arm64" => Ok(TargetPlatform::LinuxArm64),
            "windows-arm64" => Ok(TargetPlatform::WindowsArm64),
            "darwin-arm64" | "macos-arm64" => Ok(TargetPlatform::DarwinArm64),
            _ => Err(format!("Unknown target platform: {}", s)),
        }
    }

    fn goos_goarch(&self) -> (&str, &str) {
        match self {
            TargetPlatform::Linux => ("linux", "amd64"),
            TargetPlatform::Windows => ("windows", "amd64"),
            TargetPlatform::Darwin => ("darwin", "amd64"),
            TargetPlatform::LinuxArm64 => ("linux", "arm64"),
            TargetPlatform::WindowsArm64 => ("windows", "arm64"),
            TargetPlatform::DarwinArm64 => ("darwin", "arm64"),
        }
    }
}

#[derive(Debug, Clone)]
enum BuildMode {
    Executable,     // default - regular executable
    Library,        // c-shared - C shared library
}

impl BuildMode {
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "bin" => Ok(BuildMode::Executable),
            "lib" => Ok(BuildMode::Library),
            _ => Err(format!("Unknown build mode: {} (use 'bin' or 'lib')", s)),
        }
    }

    fn go_buildmode(&self) -> Option<&'static str> {
        match self {
            BuildMode::Executable => None, // default mode
            BuildMode::Library => Some("c-shared"),
        }
    }
}

#[derive(Debug, Clone)]
enum OptimizationLevel {
    None,    // -gcflags="-N -l"
    Basic,   // default
    Full,    // -ldflags="-s -w"
}

impl OptimizationLevel {
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "0" | "none" => Ok(OptimizationLevel::None),
            "1" | "basic" => Ok(OptimizationLevel::Basic),
            "2" | "full" => Ok(OptimizationLevel::Full),
            _ => Err(format!("Unknown optimization level: {} (use 0, 1, or 2)", s)),
        }
    }

    fn go_flags(&self) -> Vec<String> {
        match self {
            OptimizationLevel::None => vec!["-gcflags".to_string(), "-N -l".to_string()],
            OptimizationLevel::Basic => vec![],
            OptimizationLevel::Full => vec!["-ldflags".to_string(), "-s -w".to_string()],
        }
    }
}

#[derive(Debug)]
struct BuildConfig {
    target_platform: Option<TargetPlatform>,
    build_mode: BuildMode,
    optimization_level: OptimizationLevel,
    output_name: Option<String>,
}

impl Default for BuildConfig {
    fn default() -> Self {
        BuildConfig {
            target_platform: None,
            build_mode: BuildMode::Executable,
            optimization_level: OptimizationLevel::Basic,
            output_name: None,
        }
    }
}

fn build_project_from_path<P: AsRef<Path>>(project_path: P, config: &BuildConfig) {
    let project_path = project_path.as_ref();

    println!("Building project from: {}", project_path.display());
    println!("Target platform: {:?}", config.target_platform);
    println!("Build mode: {:?}", config.build_mode);
    println!("Optimization level: {:?}", config.optimization_level);

    let filesystem = Box::new(fs::LocalFS::new(project_path.to_str().unwrap()));
    let mut instance = infer::Infer::new(filesystem);

    instance.init_std();

    let m = Module::user();

    let import = compiler::global_state::ModuleImport {
        path: "*current*".to_string(),
        name: m.id.0.to_string(),
    };

    instance.module_from_folder(import, infer::DeclareMode::Full);

    if let Some(err) = instance.first_error() {
        eprintln!("Build error: {}", err);
        std::process::exit(1);
    }

    let generated_files = {
        let std = instance.gs.get_module(&ModuleId::from_str("std")).unwrap();
        let user = instance.gs.get_module(&ModuleId::from_str("user")).unwrap();

        let mut gen = codegen::Codegen::new(instance);

        let mut files = Vec::new();
        files.extend(gen.compile_module(&std));
        files.extend(gen.compile_module(&user));
        files
    };

    emit_files(&generated_files);
    compile_generated_code(&generated_files, config, project_path);
}

fn build_single_file<P: AsRef<Path>>(file_path: P, config: &BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    let file_path = file_path.as_ref();

    if !file_path.exists() {
        return Err(format!("File does not exist: {}", file_path.display()).into());
    }

    if !file_path.extension().map_or(false, |ext| ext == "ya") {
        return Err("File must have .ya extension".into());
    }

    println!("Building file: {}", file_path.display());
    println!("Target platform: {:?}", config.target_platform);
    println!("Build mode: {:?}", config.build_mode);
    println!("Optimization level: {:?}", config.optimization_level);

    let source = std::fs::read_to_string(file_path)?;
    let parent_dir = file_path.parent().unwrap_or(Path::new("."));

    let filesystem = Box::new(fs::LocalFS::new(parent_dir.to_str().unwrap()));
    let mut instance = infer::Infer::new(filesystem);

    instance.init_std();

    let m = Module::user();
    instance.gs.set_module(m.id.clone());
    instance.gs.modules.insert(m.id.clone(), m.clone());

    let file_name = file_path.file_name().unwrap().to_str().unwrap();
    instance.file_from_source(file_name, &source);

    // run inference
    instance.declare_module(&m.id);
    instance.infer_module(&m.id);

    if let Some(err) = instance.first_error() {
        return Err(format!("Inference error: {}", err).into());
    }

    let user = instance.gs.get_module(&ModuleId::from_str("user")).unwrap();
    let mut gen = codegen::Codegen::new(instance);

    let generated_files = gen.compile_module(&user);
    emit_files(&generated_files);

    compile_generated_code(&generated_files, config, parent_dir);

    Ok(())
}

fn build_multiple_files<P: AsRef<Path>>(file_paths: &[P], config: &BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    for (_, file_path) in file_paths.iter().enumerate() {
        let file_path = file_path.as_ref();
        if let Err(e) = build_single_file(file_path, config) {
            eprintln!("Error building {}: {}", file_path.display(), e);
            return Err(e);
        }
    }

    Ok(())
}

fn emit_files(files: &[EmittedFile]) {
    for file in files {
        if let Err(e) = std::fs::write(&file.name, &file.render_source()) {
            eprintln!("Failed to compile {}: {}", file.name, e);
        }
    }
}

fn cleanup_generated_files(files: &[EmittedFile]) {
    for file in files {
        if file.name.ends_with(".go") {
            std::fs::remove_file(&file.name).unwrap()
        }
    }
}

fn ensure_build_directory(working_dir: &Path) -> Result<std::path::PathBuf, std::io::Error> {
    let build_dir = working_dir.join("build");
    if !build_dir.exists() {
        std::fs::create_dir_all(&build_dir)?;
        println!("Created build directory: {}", build_dir.display());
    }
    Ok(build_dir)
}

fn compile_generated_code(files: &[EmittedFile], config: &BuildConfig, working_dir: &Path) {
    // Create build directory
    let build_dir = match ensure_build_directory(working_dir) {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("Failed to create build directory: {}", e);
            cleanup_generated_files(files);
            return;
        }
    };

    // Find main file or use the first one
    let main_file = files
        .iter()
        .find(|f| f.name.ends_with("main.go") || f.name == "main.go")
        .or_else(|| files.first());

    let Some(main_file) = main_file else {
        eprintln!("No source files to compile");
        cleanup_generated_files(files);
        return;
    };

    let mut cmd = Command::new("go");

    // Set environment variables for cross-compilation
    if let Some(target) = &config.target_platform {
        let (goos, goarch) = target.goos_goarch();
        cmd.env("GOOS", goos);
        cmd.env("GOARCH", goarch);
    }

    match &config.build_mode {
        BuildMode::Library => {
            cmd.env("CGO_ENABLED", "1");
        }
        BuildMode::Executable => {}
    }

    // Configure build command
    cmd.arg("build");

    // Add buildmode if specified
    if let Some(buildmode) = config.build_mode.go_buildmode() {
        cmd.arg(format!("-buildmode={}", buildmode));
    }

    // Add optimization flags
    let opt_flags = config.optimization_level.go_flags();
    for flag in opt_flags {
        cmd.arg(flag);
    }

    cmd.arg("--trimpath");

    // Set output path in build directory
    let output_path = if let Some(output_name) = &config.output_name {
        build_dir.join(output_name)
    } else {
        let default_name = &main_file.name;
        build_dir.join(default_name)
    };

    cmd.arg("-o").arg(&output_path);

    // Add the main file
    cmd.arg(&main_file.name);

    // Set working directory
    cmd.current_dir(working_dir);

    // Execute the command
    match cmd.output() {
        Ok(output) => {
            if output.status.success() {
                println!("Compilation successful!");
                cleanup_generated_files(files);
            } else {
                eprintln!("Compilation failed!");
                if !output.stderr.is_empty() {
                    eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
                }
                if !output.stdout.is_empty() {
                    eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
                }
                cleanup_generated_files(files);
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Failed to execute compiler: {}", e);
            cleanup_generated_files(files);
            std::process::exit(1);
        }
    }
}

fn help() {
    println!(
        "
Commands:
  build [options] [path/files...]     compile .ya files

Options:
  --target <platform>      target platform (linux, windows, darwin,
                          linux-arm64, windows-arm64, darwin-arm64)
                          [default: current platform]
  --mode <mode>           build mode (bin, lib) [default: bin]
  --opt <level>           optimization level (0=none, 1=basic, 2=full) [default: 1]
  --output <name>         output file name

Examples:
  build                                    # compile current directory
  build src/                               # compile directory
  build main.ya                            # compile single file
  build file1.ya file2.ya                  # compile multiple files
  build --target windows --mode bin        # compile executable for Windows
  build --target linux-arm64 --opt 2       # optimize for Linux ARM64
"
    );
}

fn parse_args(args: &[String]) -> Result<(BuildConfig, Vec<String>), String> {
    let mut config = BuildConfig::default();
    let mut paths = Vec::new();
    let mut i = 2; // Skip program name and "build" command

    while i < args.len() {
        match args[i].as_str() {
            "--target" => {
                i += 1;
                if i >= args.len() {
                    return Err("--target requires a value".to_string());
                }
                config.target_platform = Some(TargetPlatform::from_str(&args[i])?);
            }
            "--mode" => {
                i += 1;
                if i >= args.len() {
                    return Err("--mode requires a value".to_string());
                }
                config.build_mode = BuildMode::from_str(&args[i])?;
            }
            "--opt" => {
                i += 1;
                if i >= args.len() {
                    return Err("--opt requires a value".to_string());
                }
                config.optimization_level = OptimizationLevel::from_str(&args[i])?;
            }
            "--output" => {
                i += 1;
                if i >= args.len() {
                    return Err("--output requires a value".to_string());
                }
                config.output_name = Some(args[i].clone());
            }
            arg if arg.starts_with("--") => {
                return Err(format!("Unknown option: {}", arg));
            }
            _ => {
                paths.push(args[i].clone());
            }
        }
        i += 1;
    }

    Ok((config, paths))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        help();
        return;
    }

    let cmd = &args[1];

    match cmd.as_str() {
        "build" => {
            let (config, paths) = match parse_args(&args) {
                Ok((config, paths)) => (config, paths),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    help();
                    std::process::exit(1);
                }
            };

            if paths.is_empty() {
                // No paths specified, build current directory
                build_project_from_path(".", &config);
            } else if paths.len() == 1 {
                // Single path - could be directory or file
                let path = Path::new(&paths[0]);

                if path.is_dir() {
                    build_project_from_path(path, &config);
                } else if path.exists() && path.extension().map_or(false, |ext| ext == "ya") {
                    if let Err(e) = build_single_file(path, &config) {
                        eprintln!("Build failed: {}", e);
                        std::process::exit(1);
                    }
                } else {
                    eprintln!("Invalid path or file: {}", path.display());
                    std::process::exit(1);
                }
            } else {
                // Multiple paths - treat as multiple files
                let file_paths: Vec<&String> = paths.iter().collect();

                // Validate all files exist and have .ya extension
                for file_path in &file_paths {
                    let path = Path::new(file_path);
                    if !path.exists() {
                        eprintln!("File does not exist: {}", path.display());
                        std::process::exit(1);
                    }
                    if !path.extension().map_or(false, |ext| ext == "ya") {
                        eprintln!("File must have .ya extension: {}", path.display());
                        std::process::exit(1);
                    }
                }

                if let Err(e) = build_multiple_files(&file_paths, &config) {
                    eprintln!("Build failed: {}", e);
                    std::process::exit(1);
                }
            }
        }

        _ => {
            help();
        }
    }
}