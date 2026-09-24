use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::{exit, Command},
};

use azula_codegen::prelude::{Backend, Codegen, OptimizationLevel};
use azula_codegen_llvm::prelude::LLVMCodegen;
use azula_parser::prelude::{Lexer, Parser};
use azula_typecheck::prelude::Typechecker;

mod modules;
use modules::{collect_items, import_line, rewrite, ModuleInfo};
// use azula_vm::VM;
use clap::{StructOpt, Subcommand};

/// Azula command line
#[derive(clap::Parser, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct AzulaCLI {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Run {
        files: Vec<String>,

        #[clap(long)]
        release: bool,

        #[clap(long)]
        print_azula_ir: bool,
    },
    Build {
        files: Vec<String>,

        #[clap(long)]
        target: Option<String>,

        #[clap(long)]
        emit_llvm: bool,

        #[clap(long)]
        release: bool,

        #[clap(long)]
        print_azula_ir: bool,
    },
}

pub fn run() {
    let args = AzulaCLI::parse();

    match &args.command {
        Commands::Run {
            files,
            release,
            print_azula_ir,
        } => {
            let result = build(files, ".build/", None, false, *release, *print_azula_ir);

            let status = Command::new(format!("./.build/{}", result))
                .status()
                .unwrap();
            exit(status.code().unwrap_or(1));
        }
        Commands::Build {
            files,
            target,
            emit_llvm,
            release,
            print_azula_ir,
        } => {
            build(
                files,
                "",
                target.as_ref(),
                *emit_llvm,
                *release,
                *print_azula_ir,
            );
        }
    }
}

const STDLIB: &[(&str, &str)] = &[
    ("stdlib/libc.azl", include_str!("../../stdlib/libc.azl")),
    ("stdlib/option.azl", include_str!("../../stdlib/option.azl")),
    ("stdlib/string.azl", include_str!("../../stdlib/string.azl")),
    ("stdlib/vec.azl", include_str!("../../stdlib/vec.azl")),
    ("stdlib/map.azl", include_str!("../../stdlib/map.azl")),
    ("stdlib/interfaces.azl", include_str!("../../stdlib/interfaces.azl")),
    ("stdlib/result.azl", include_str!("../../stdlib/result.azl")),
    ("stdlib/io.azl", include_str!("../../stdlib/io.azl")),
    ("stdlib/iter.azl", include_str!("../../stdlib/iter.azl")),
    ("stdlib/math.azl", include_str!("../../stdlib/math.azl")),
];

/// Standard library modules that programs import on request, as
/// `import "std/net.azl" as net`
const STD_MODULES: &[(&str, &str)] = &[("net.azl", include_str!("../../stdlib/net.azl"))];

/// The combined program source, plus a record of which file and line every
/// line of it came from (so errors can point at the original location).
#[derive(Default)]
struct Source {
    text: String,
    lines: Vec<(String, usize)>,
}

impl Source {
    fn push_line(&mut self, line: &str, file: &str, line_number: usize) {
        self.text.push_str(line);
        self.text.push('\n');
        self.lines.push((file.to_string(), line_number));
    }

    fn push_file(&mut self, file: &str, contents: &str) {
        for (index, line) in contents.lines().enumerate() {
            self.push_line(line, file, index + 1);
        }
    }

    fn locate(&self, line: usize) -> (String, usize) {
        self.lines
            .get(line.saturating_sub(1))
            .cloned()
            .unwrap_or_else(|| ("<unknown>".to_string(), line))
    }
}

/// Recursively read `path` and all its `import "..."` dependencies into
/// `source` (dependencies first). `seen` prevents duplicate inclusion. A file
/// imported `as name` is a module: see modules.rs.
fn resolve_imports(
    path: &Path,
    alias: Option<&str>,
    seen: &mut HashMap<PathBuf, ModuleInfo>,
    source: &mut Source,
    // The contents of a standard library module, which isn't read from disk
    std_module: Option<&str>,
) -> ModuleInfo {
    let canonical = if std_module.is_some() {
        path.to_path_buf()
    } else {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    };
    if let Some(info) = seen.get(&canonical) {
        return info.clone();
    }

    let src = std_module.map(str::to_string).unwrap_or_else(|| {
        fs::read_to_string(path).unwrap_or_else(|_| {
            eprintln!("Could not read file: {}", path.display());
            exit(1);
        })
    });

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file = path.display().to_string();
    let (items, public) = collect_items(&src);
    let info = ModuleInfo {
        prefix: alias.map(|a| format!("{}__", a)).unwrap_or_default(),
        items,
        public,
        file: file.clone(),
    };
    seen.insert(canonical, info.clone());

    // Dependencies first, noting the names modules are imported as
    let mut aliases = HashMap::new();
    for line in src.lines() {
        if let Some((import_path, name)) = import_line(line) {
            // `std/...` names a module of the (embedded) standard library
            let module = match import_path.strip_prefix("std/") {
                Some(std_name) => {
                    let contents = match STD_MODULES.iter().find(|(file, _)| *file == std_name) {
                        Some((_, contents)) => contents,
                        None => {
                            let line = src[..src.find(line).unwrap_or(0)].matches('\n').count() + 1;
                            eprintln!("error: {}:{}: there's no standard library module `{}`", file, line, import_path);
                            exit(1);
                        }
                    };
                    resolve_imports(Path::new(&import_path), name.as_deref(), seen, source, Some(contents))
                }
                None => resolve_imports(&dir.join(&import_path), name.as_deref(), seen, source, None),
            };
            if let Some(name) = name {
                aliases.insert(name, module);
            }
        }
    }

    let own = if info.prefix.is_empty() { None } else { Some(&info) };
    let text = match rewrite(&src, own, &aliases) {
        Ok(text) => text,
        Err((offset, message)) => {
            let line = src[..offset].matches('\n').count() + 1;
            eprintln!("error: {}:{}: {}", file, line, message);
            exit(1);
        }
    };
    for (index, line) in text.lines().enumerate() {
        // Import lines are blanked, keeping the line count of this file intact
        if import_line(line).is_some() {
            source.push_line("", &file, index + 1);
        } else {
            source.push_line(line, &file, index + 1);
        }
    }
    info
}

fn build(
    files: &[String],
    destination: &str,
    target: Option<&String>,
    emit_llvm: bool,
    release: bool,
    print_azula_ir: bool,
) -> String {
    let mut source = Source::default();
    for (file, contents) in STDLIB {
        source.push_file(file, contents);
    }
    let mut seen = HashMap::new();
    for f in files {
        resolve_imports(Path::new(f), None, &mut seen, &mut source, None);
    }

    let input = source.text.clone();
    let locate = |line: usize| source.locate(line);

    let primary = files.last().unwrap();

    let lexer: Lexer = input.as_str().into();
    let mut parser = Parser::new(input.as_str(), lexer);
    let parsed = parser.parse();
    for error in &parser.errors {
        error.print_stdout_mapped(&input, &locate);
    }

    if !parser.errors.is_empty() {
        exit(1);
    }

    let mut typecheck = Typechecker::new(parsed);
    let result = typecheck.typecheck();
    for err in &typecheck.errors {
        err.print_stdout_mapped(&input, &locate);
    }

    let root = match result {
        Ok(root) if typecheck.errors.is_empty() => root,
        _ => exit(1),
    };

    let name = if destination.is_empty() {
        primary.trim_end_matches(".azl").to_string()
    } else {
        Path::new(primary.trim_end_matches(".azl"))
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    };

    let mut codegen = Codegen::new(&name, root);
    codegen.codegen();
    codegen.insert_implicit_return();

    if print_azula_ir {
        println!("{}", codegen.module);
    }

    if let Err(e) = LLVMCodegen::codegen(
        &name,
        destination,
        emit_llvm,
        target,
        if release {
            OptimizationLevel::Aggressive
        } else {
            OptimizationLevel::Default
        },
        codegen.module,
    ) {
        eprintln!("{}", e);
        exit(1);
    }

    name
}
