use cog::backend::CompiledProgram;
use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{build, bytecode, config, frontend, interpreter, parser, project};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Instant;

mod cli_output;
use cli_output::Color;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Check,
    Run,
    Build,
}

#[derive(Default)]
struct Options {
    debug: bool,
    allow_asm: bool,
    restricted: bool,
    target: Option<String>,
    use_modules: Vec<String>,
    locked: bool,
    output: Option<PathBuf>,
    program_args: Vec<OsString>,
    program_args_separator: bool,
    plugins: Vec<PathBuf>,
}

fn main() {
    let mut color = Color::default();
    if let Err(error) = command(&mut color) {
        color.error(&error);
        std::process::exit(1);
    }
}

fn command(color: &mut Color) -> Result<(), String> {
    let mut options = Options::default();
    let mut action = None;
    let mut target = None;
    let mut args = std::env::args_os().skip(1).peekable();
    while let Some(argument) = args.next() {
        if argument == OsStr::new("--") {
            options.program_args_separator = true;
            options.program_args = args.collect();
            break;
        }
        let arg = argument
            .into_string()
            .map_err(|_| "CLI options and source paths before `--` must be UTF-8".to_owned())?;
        match arg.as_str() {
            "--debug" | "-v" => options.debug = true,
            "--allow-asm" => options.allow_asm = true,
            "--restricted" => options.restricted = true,
            "--target" => options.target = Some(required_cli_arg(&mut args, "--target")?),
            "--use" => options
                .use_modules
                .push(required_cli_arg(&mut args, "--use")?),
            "--locked" => options.locked = true,
            "--plugin" => options.plugins.push(
                args.next()
                    .ok_or("--plugin requires a KDL manifest path")?
                    .into(),
            ),
            "--color" => *color = Color::parse(&required_cli_arg(&mut args, "--color")?)?,
            "--output" | "-o" => {
                options.output = Some(args.next().ok_or("--output requires a .cogb path")?.into())
            }
            "--version" | "-V" => {
                println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            "init" | "check" | "run" | "build" | "eval" if action.is_none() => action = Some(arg),
            _ if arg.starts_with('-')
                && !(action.as_deref() == Some("eval") && target.is_none()) =>
            {
                return Err(format!("unknown option `{arg}`"));
            }
            _ if target.is_none() => target = Some(arg),
            _ => return Err(format!("unexpected argument `{arg}`")),
        }
    }
    if action.is_none() && target.is_none() {
        print_usage();
        return Err("expected a command or a source/bytecode path".into());
    }
    let action = action.as_deref().unwrap_or("run");
    if options.program_args_separator && action != "run" {
        return Err("program arguments after `--` are only valid with `run`".into());
    }
    if options.output.is_some() && action != "build" {
        return Err("--output is only valid with `build`".into());
    }
    if !options.use_modules.is_empty() && action != "eval" {
        return Err("--use is only valid with `eval`".into());
    }
    if action == "eval" {
        if !options.plugins.is_empty() {
            return Err("--plugin is not supported with `eval`".into());
        }
        if options.target.is_some() || options.locked || options.allow_asm {
            return Err("--target, --locked, and --allow-asm are not valid with `eval`".into());
        }
        let expression = target
            .as_deref()
            .filter(|expression| !expression.trim().is_empty())
            .ok_or("eval requires one Cog expression; usage: cog eval [options] <expression>")?;
        return eval_expression(expression, &options, color);
    }
    if action == "init" {
        if !options.plugins.is_empty() {
            return Err("--plugin is only valid with check, build or run".into());
        }
        let directory = target
            .as_deref()
            .filter(|directory| !directory.is_empty())
            .ok_or("init requires a directory; usage: cog init <directory>")?;
        return init_project(Path::new(directory));
    }
    let target = Path::new(target.as_deref().unwrap_or("."));
    let mode = match action {
        "check" => Mode::Check,
        "build" => Mode::Build,
        _ => Mode::Run,
    };
    if !options.plugins.is_empty() {
        return command_with_plugins(target, mode, options, color);
    }
    let bytecode_input =
        !target.is_dir() && target.extension().and_then(|s| s.to_str()) == Some("cogb");
    let started = Instant::now();
    let (compiled, reused, destination, warnings) =
        prepare(target, mode, &options).map_err(|error| {
            let action = if bytecode_input {
                "load"
            } else if mode == Mode::Check {
                "check"
            } else {
                "build"
            };
            cli_output::preparation_error(error, action, target, started.elapsed())
        })?;
    let elapsed = started.elapsed();
    if !warnings.is_empty() {
        color.warning(&frontend::render(&warnings));
    }
    if options.debug {
        println!("bytecode\n{}", compiled.disassemble());
    }
    match mode {
        Mode::Check => {
            color.status("checked", target, elapsed);
            Ok(())
        }
        Mode::Run => {
            if !bytecode_input && !reused {
                color.status("built", &destination, elapsed);
            }
            let capabilities = if options.restricted {
                VmCapabilities {
                    allow_print: true,
                    allow_read: true,
                    ..VmCapabilities::restricted()
                }
            } else {
                VmCapabilities::default()
            };
            let mut stdout = std::io::stdout();
            let mut context = RuntimeContext::new(&mut stdout, capabilities);
            context.set_process_arguments(options.program_args);
            interpreter::run_with_context(&compiled, &mut context).map_err(|error| {
                if let Some(location) = context.last_source_location() {
                    format!(
                        "runtime error at {}:{}:{}: {error}",
                        location.file.escape_debug(),
                        location.line,
                        location.column
                    )
                } else {
                    format!("runtime error: {error}")
                }
            })
        }
        Mode::Build => {
            color.status(
                if reused { "finished" } else { "built" },
                &destination,
                elapsed,
            );
            Ok(())
        }
    }
}

fn eval_expression(expression: &str, options: &Options, color: &Color) -> Result<(), String> {
    let mut source = String::from("include std;\nuse io;\n");
    let mut imports: Vec<String> = Vec::new();
    for module in &options.use_modules {
        if !is_identifier(module) {
            return Err(format!(
                "--use expects a standard-library module name, got `{module}`"
            ));
        }
        if module != "io" && !imports.contains(module) {
            source.push_str("use ");
            source.push_str(module);
            source.push_str(";\n");
            imports.push(module.clone());
        }
    }
    source.push_str("\nfn main() { io::println((");
    source.push_str(expression);
    source.push_str("))!!; }\n");

    let (compiled, warnings) = project::compile_source_with_warnings("eval.cog", &source)?;
    if !warnings.is_empty() {
        color.warning(&frontend::render(&warnings));
    }
    if options.debug {
        println!("bytecode\n{}", compiled.disassemble());
    }
    let capabilities = if options.restricted {
        VmCapabilities {
            allow_print: true,
            allow_read: true,
            ..VmCapabilities::restricted()
        }
    } else {
        VmCapabilities::default()
    };
    let mut stdout = std::io::stdout();
    let mut context = RuntimeContext::new(&mut stdout, capabilities);
    interpreter::run_with_context(&compiled, &mut context).map_err(|error| {
        if let Some(location) = context.last_source_location() {
            format!(
                "runtime error at {}:{}:{}: {error}",
                location.file.escape_debug(),
                location.line,
                location.column
            )
        } else {
            format!("runtime error: {error}")
        }
    })
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn prepare(
    target: &Path,
    mode: Mode,
    options: &Options,
) -> Result<
    (
        CompiledProgram,
        bool,
        PathBuf,
        Vec<cog::diagnostic::Diagnostic>,
    ),
    String,
> {
    let bytecode_input =
        !target.is_dir() && target.extension().and_then(|s| s.to_str()) == Some("cogb");
    let prepared = if target.is_dir() || target.extension().and_then(|s| s.to_str()) == Some("cog")
    {
        Some(
            project::prepare(target, options.target.as_deref(), options.locked)
                .map_err(|errors| frontend::render(&errors))?,
        )
    } else {
        None
    };
    let artifact_name = prepared
        .as_ref()
        .map(|p| p.artifact_name.as_str())
        .unwrap_or("main");
    let destination = options.output.clone().unwrap_or_else(|| {
        if target.is_dir() {
            target.join(format!("target/{artifact_name}.cogb"))
        } else {
            target.with_extension("cogb")
        }
    });
    if mode == Mode::Build && destination.extension().and_then(|s| s.to_str()) != Some("cogb") {
        return Err("bytecode output must have the .cogb extension".into());
    }
    let (compiled, reused, warnings) = if let Some(prepared) = prepared {
        if mode == Mode::Check {
            let loaded = prepared.resolve();
            let (compiled, warnings) = project::compile_with_inputs_and_warnings(
                loaded.program,
                loaded.diagnostics,
                &loaded.names,
                &loaded.build_inputs,
            )?;
            (compiled, false, warnings)
        } else {
            let (built, warnings) = build::project_with_warnings(prepared, &destination)?;
            (built.program, built.reused, warnings)
        }
    } else {
        let mut compiled = compile_file(target, options)?;
        if mode == Mode::Build || (mode == Mode::Run && !bytecode_input) {
            build::write_bytecode(&destination, &compiled)?;
            if mode == Mode::Run {
                compiled = load_bytecode(&destination)?;
            }
        }
        (compiled, false, Vec::new())
    };
    Ok((compiled, reused, destination, warnings))
}

fn print_usage() {
    eprintln!("usage: cog [check|run|build] [path] [options] [-- program-arguments...]");
    eprintln!("       cog eval [--debug] [--restricted] [--use <module>]... <expression>");
    eprintln!("       cog --version|-V");
    eprintln!("       cog build <file.cog|project> [-o output.cogb]");
    eprintln!("       cog run [file.cog|project]  (reuse or build .cogb, then execute it)");
    eprintln!("       cog run <file.cogb>  (no source files or compilation required)");
    eprintln!("       cog init <directory>");
    eprintln!("       eval compiles and runs one expression; --debug prints its bytecode");
    eprintln!("       --allow-asm  enable the standalone assembler for VM diagnostics");
    eprintln!("       --restricted  allow only stdin/stdout runtime host effects");
    eprintln!("       --plugin <manifest.kdl>  explicit trusted native plugin (repeatable)");
    eprintln!("       `--` passes the remaining arguments to process::args() during `run`");
    eprintln!("       --version, -V  print the Cog version");
    eprintln!("       --color <auto|always|never>  diagnostic and status colors (default: auto)");
}

fn command_with_plugins(
    target: &Path,
    mode: Mode,
    options: Options,
    color: &Color,
) -> Result<(), String> {
    use cog::sdk::{Engine, Permissions, PluginManifest, RunContext};
    let started = Instant::now();
    let mut engine = Engine::new();
    for path in &options.plugins {
        let manifest = PluginManifest::read(path).map_err(|error| error.to_string())?;
        if mode == Mode::Run {
            // --plugin is explicit user authorization to execute this trusted
            // native library with process authority. Check/build never load it.
            unsafe { manifest.load(&mut engine, path.parent().unwrap_or(Path::new("."))) }
                .map_err(|error| error.to_string())?;
        } else {
            manifest
                .register_metadata(&mut engine)
                .map_err(|error| error.to_string())?;
        }
    }
    let bytecode_input = !target.is_dir() && target.extension() == Some(OsStr::new("cogb"));
    let (program, reused, destination) = if bytecode_input {
        let file = std::fs::File::open(target).map_err(|error| error.to_string())?;
        let program = engine.read(file).map_err(|error| error.to_string())?;
        let destination = options
            .output
            .clone()
            .unwrap_or_else(|| target.to_path_buf());
        if mode == Mode::Build {
            if destination.extension() != Some(OsStr::new("cogb")) {
                return Err("bytecode output must have the .cogb extension".into());
            }
            program
                .write(&destination)
                .map_err(|error| error.to_string())?;
        }
        (program, true, destination)
    } else {
        if !target.is_dir() && target.extension() != Some(OsStr::new("cog")) {
            return Err("plugins require a .cog source, project directory or .cogb file".into());
        }
        let prepared = project::prepare(target, options.target.as_deref(), options.locked)
            .map_err(|errors| frontend::render(&errors))?;
        let destination = options.output.clone().unwrap_or_else(|| {
            if target.is_dir() {
                target.join(format!("target/{}.cogb", prepared.artifact_name))
            } else {
                target.with_extension("cogb")
            }
        });
        if mode == Mode::Check {
            (
                engine
                    .compile_project(target, options.target.as_deref(), options.locked)
                    .map_err(|error| error.to_string())?,
                false,
                destination,
            )
        } else {
            if destination.extension() != Some(OsStr::new("cogb")) {
                return Err("bytecode output must have the .cogb extension".into());
            }
            let built = engine
                .build_project(
                    target,
                    &destination,
                    options.target.as_deref(),
                    options.locked,
                )
                .map_err(|error| error.to_string())?;
            (built.program, built.reused, destination)
        }
    };
    if options.debug {
        println!("bytecode\n{}", program.disassemble());
    }
    match mode {
        Mode::Check => {
            color.status("checked", target, started.elapsed());
            Ok(())
        }
        Mode::Build => {
            color.status(
                if reused { "finished" } else { "built" },
                &destination,
                started.elapsed(),
            );
            Ok(())
        }
        Mode::Run => {
            if !bytecode_input && !reused {
                color.status("built", &destination, started.elapsed());
            }
            let mut stdout = std::io::stdout();
            let mut context = RunContext::new(&mut stdout);
            if options.restricted {
                context = context.with_permissions(
                    Permissions::none().allow(cog::sdk::Effects::OUTPUT | cog::sdk::Effects::INPUT),
                );
            }
            context.set_process_arguments(options.program_args);
            program
                .run_in(&mut context)
                .map_err(|error| format!("runtime error: {error}"))
        }
    }
}

fn required_cli_arg(
    args: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<String, String> {
    let argument = args
        .next()
        .ok_or_else(|| format!("{option} requires a value"))?;
    argument
        .into_string()
        .map_err(|_| format!("the value for {option} must be UTF-8"))
}

fn init_project(root: &Path) -> Result<(), String> {
    if [
        "package.kdl",
        "dependencies.kdl",
        "build.kdl",
        "main.cog",
        "src/main.cog",
    ]
    .iter()
    .any(|p| root.join(p).exists())
    {
        return Err("init requires a new project; existing files were preserved".into());
    }
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create project: {e}"))?;
    let directory = root
        .canonicalize()
        .map_err(|e| format!("cannot resolve project directory: {e}"))?;
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| config::valid_package_name(name))
        .ok_or("project directory name is not a valid UTF-8 package name")?;
    let mut package = kdl::KdlNode::new("package");
    package.push(name);
    package.insert("version", "0.1.0");
    let manifest =
        include_str!("templates/package.kdl").replace("{{package}}", &package.to_string());
    std::fs::create_dir_all(root.join("src")).map_err(|e| format!("cannot create project: {e}"))?;
    for (path, content) in [
        ("package.kdl", manifest.as_str()),
        (
            "dependencies.kdl",
            include_str!("templates/dependencies.kdl"),
        ),
        ("build.kdl", include_str!("templates/build.kdl")),
        (
            "src/main.cog",
            "include std;\nuse io;\n\nfn main() -> nil {\n    io::println(\"Hello, Cog!\")!!;\n}\n",
        ),
    ] {
        use std::io::Write;
        let mut file = std::fs::File::create_new(root.join(path))
            .map_err(|e| format!("cannot create {path}: {e}"))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("cannot write {path}: {e}"))?;
    }
    println!("created project at {}", root.display());
    Ok(())
}

fn compile_file(path: &Path, options: &Options) -> Result<CompiledProgram, String> {
    let extension = path.extension().and_then(|s| s.to_str());
    if extension == Some("cogb") {
        return load_bytecode(path);
    }
    let bytes = cog::limits::read_source(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let source = String::from_utf8(bytes)
        .map_err(|_| format!("source {} is not valid UTF-8", path.display()))?;
    match extension {
        Some("asm") if options.allow_asm => {
            parser::assemble(&source).map_err(|e| format!("assembler error: {e}"))
        }
        Some("asm") => Err("standalone assembly requires --allow-asm".into()),
        _ => Err(format!(
            "expected a .cog or .cogb file, got {}",
            path.display()
        )),
    }
}

fn load_bytecode(path: &Path) -> Result<CompiledProgram, String> {
    let file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if file
        .metadata()
        .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?
        .len()
        > bytecode::MAX_FILE_BYTES as u64
    {
        return Err("bytecode file exceeds the format limit".into());
    }
    bytecode::read(file)
}
