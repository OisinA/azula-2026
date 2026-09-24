use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::{exit, Command},
};

use azula_codegen::prelude::{Backend, Codegen, OptimizationLevel};
use azula_codegen_llvm::prelude::LLVMCodegen;
use azula_parser::prelude::{Lexer, Parser};
use azula_typecheck::prelude::Typechecker;
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
];

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
/// `source`. `seen` prevents duplicate inclusion.
fn resolve_imports(path: &Path, seen: &mut HashSet<PathBuf>, source: &mut Source) {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if seen.contains(&canonical) {
        return;
    }
    seen.insert(canonical.clone());

    let src = fs::read_to_string(path).unwrap_or_else(|_| {
        eprintln!("Could not read file: {}", path.display());
        exit(1);
    });

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file = path.display().to_string();

    for (index, line) in src.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("import \"") {
            if let Some(import_path) = rest.strip_suffix('"') {
                resolve_imports(&dir.join(import_path), seen, source);
                // Keep the line count of this file intact.
                source.push_line("", &file, index + 1);
                continue;
            }
        }
        source.push_line(line, &file, index + 1);
    }
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
    let mut seen = HashSet::new();
    for f in files {
        resolve_imports(Path::new(f), &mut seen, &mut source);
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
