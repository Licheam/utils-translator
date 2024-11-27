#![allow(clippy::too_many_arguments)]
#![feature(drain_filter)]

mod diagnostics;

pub mod build_files;
pub mod c_ast;
pub mod cfg;
mod compile_cmds;
pub mod convert_type;
pub mod renamer;
pub mod rust_ast;
pub mod translator;
pub mod with_stmts;

use std::collections::{binary_heap, HashSet};
use std::fs::{self, File};
use std::io;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::process;

use build_files::get_lib;
use failure::Error;
use itertools::Itertools;
use log::{info, warn};
use regex::Regex;
use serde_derive::Serialize;

use crate::c_ast::Printer;
use crate::c_ast::*;
pub use crate::diagnostics::Diagnostic;
use c2rust_ast_exporter as ast_exporter;

use crate::build_files::{emit_build_files, get_build_dir, get_build_dir_raw, CrateConfig};
use crate::compile_cmds::get_compile_commands;
use crate::convert_type::RESERVED_NAMES;
pub use crate::translator::ReplaceMode;
use std::prelude::v1::Vec;

type PragmaVec = Vec<(&'static str, Vec<&'static str>)>;
type PragmaSet = indexmap::IndexSet<(&'static str, &'static str)>;
type CrateSet = indexmap::IndexSet<ExternCrate>;
type TranspileResult = Result<(PathBuf, PragmaVec, CrateSet), ()>;

use deps_builder::{build_dependency, DependencyGraph, DependencyInfo, DependencySymbol};

/// Configuration settings for the translation process
#[derive(Debug, Clone)]
pub struct TranspilerConfig {
    // Debug output options
    pub dump_untyped_context: bool,
    pub dump_typed_context: bool,
    pub pretty_typed_context: bool,
    pub dump_function_cfgs: bool,
    pub json_function_cfgs: bool,
    pub dump_cfg_liveness: bool,
    pub dump_structures: bool,
    pub verbose: bool,
    pub debug_ast_exporter: bool,

    // Options that control translation
    pub incremental_relooper: bool,
    pub fail_on_multiple: bool,
    pub filter: Option<Regex>,
    pub debug_relooper_labels: bool,
    pub prefix_function_names: Option<String>,
    pub translate_asm: bool,
    pub use_c_loop_info: bool,
    pub use_c_multiple_info: bool,
    pub simplify_structures: bool,
    pub panic_on_translator_failure: bool,
    pub emit_modules: bool,
    pub fail_on_error: bool,
    pub replace_unsupported_decls: ReplaceMode,
    pub translate_valist: bool,
    pub overwrite_existing: bool,
    pub reduce_type_annotations: bool,
    pub reorganize_definitions: bool,
    pub enabled_warnings: HashSet<Diagnostic>,
    pub emit_no_std: bool,
    pub emit_no_lib: bool,
    pub output_dir: Option<PathBuf>,
    pub translate_const_macros: bool,
    pub translate_fn_macros: bool,
    pub disable_refactoring: bool,
    pub preserve_unused_functions: bool,
    pub log_level: log::LevelFilter,

    // Options that control build files
    /// Emit `Cargo.toml` and `lib.rs`
    pub emit_build_files: bool,
    pub emit_binaries: bool,
    /// Names of translation units containing main functions that we should make
    /// into binaries
    pub binaries: Vec<String>,
    pub detect_binaries: bool,
    pub dependency_file: PathBuf,
    pub fuzz_depends_level: usize,
}

impl TranspilerConfig {
    fn binary_name_from_path(file: &Path) -> String {
        let file = Path::new(file.file_stem().unwrap());
        get_module_name(file, false, false, false).unwrap()
    }

    fn is_binary(&self, dependency_info: &DependencyInfo) -> bool {
        let file = dependency_info.input_path.as_ref();
        let module_name = Self::binary_name_from_path(file);
        self.binaries.contains(&module_name)
            || (self.detect_binaries
                && dependency_info
                    .defined
                    .iter()
                    .any(|symbol| symbol.name.contains("main")))
    }

    fn check_if_all_binaries_used(
        &self,
        transpiled_modules: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> bool {
        let module_names = transpiled_modules
            .into_iter()
            .map(|module| Self::binary_name_from_path(module.as_ref()))
            .collect::<HashSet<_>>();
        let mut ok = true;
        for binary in &self.binaries {
            if !module_names.contains(binary) {
                ok = false;
                warn!("binary not used: {binary}");
            }
        }
        if !ok {
            let module_names = module_names.iter().format(", ");
            info!("candidate modules for binaries are: {module_names}");
        }
        ok
    }

    fn crate_name(&self) -> String {
        self.output_dir
            .as_ref()
            .and_then(|x| x.file_name().map(|x| x.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "c2rust_out".into())
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExternCrate {
    C2RustBitfields,
    C2RustAsmCasts,
    F128,
    NumTraits,
    Memoffset,
    Libc,
}

#[derive(Serialize)]
struct ExternCrateDetails {
    name: &'static str,
    ident: String,
    macro_use: bool,
    version: &'static str,
}

impl ExternCrateDetails {
    fn new(name: &'static str, version: &'static str, macro_use: bool) -> Self {
        Self {
            name,
            ident: name.replace('-', "_"),
            macro_use,
            version,
        }
    }
}

impl From<ExternCrate> for ExternCrateDetails {
    fn from(extern_crate: ExternCrate) -> Self {
        match extern_crate {
            ExternCrate::C2RustBitfields => Self::new("c2rust-bitfields", "0.3", true),
            ExternCrate::C2RustAsmCasts => Self::new("c2rust-asm-casts", "0.2", true),
            ExternCrate::F128 => Self::new("f128", "0.2", false),
            ExternCrate::NumTraits => Self::new("num-traits", "0.2", true),
            ExternCrate::Memoffset => Self::new("memoffset", "0.5", true),
            ExternCrate::Libc => Self::new("libc", "0.2", false),
        }
    }
}

fn char_to_ident(c: char) -> char {
    if c.is_alphanumeric() {
        c
    } else {
        '_'
    }
}

fn str_to_ident<S: AsRef<str>>(s: S) -> String {
    s.as_ref().chars().map(char_to_ident).collect()
}

/// Make sure that name:
/// - does not contain illegal characters,
/// - does not clash with reserved keywords.
fn str_to_ident_checked(filename: &Option<String>, check_reserved: bool) -> Option<String> {
    // module names cannot contain periods or dashes
    filename.as_ref().map(str_to_ident).map(|module| {
        // make sure the module name does not clash with keywords
        if check_reserved && RESERVED_NAMES.contains(&module.as_str()) {
            format!("r#{}", module)
        } else {
            module
        }
    })
}

fn get_module_name(
    file: &Path,
    check_reserved: bool,
    keep_extension: bool,
    full_path: bool,
) -> Option<String> {
    let is_rs = file.extension().map(|ext| ext == "rs").unwrap_or(false);
    let fname = if is_rs {
        file.file_stem()
    } else {
        file.file_name()
    };
    let fname = &fname.unwrap().to_str().map(String::from);
    let mut name = str_to_ident_checked(fname, check_reserved).unwrap();
    if keep_extension && is_rs {
        name.push_str(".rs");
    }
    let file = if full_path {
        file.with_file_name(name)
    } else {
        Path::new(&name).to_path_buf()
    };
    file.to_str().map(String::from)
}

/// Main entry point to transpiler. Called from CLI tools with the result of
/// clap::App::get_matches().
pub fn transpile(tcfg: TranspilerConfig, cc_db: &Path, extra_clang_args: &[&str]) {
    let dependency_infos = export(tcfg.clone(), cc_db, extra_clang_args);
    let dependency_graph = build_dependency(dependency_infos, tcfg.fuzz_depends_level);

    diagnostics::init(tcfg.enabled_warnings.clone(), tcfg.log_level);

    let lcmds = get_compile_commands(cc_db, &tcfg.filter).unwrap_or_else(|_| {
        panic!(
            "Could not parse compile commands from {}",
            cc_db.to_string_lossy()
        )
    });

    // Specify path to system include dir on macOS 10.14 and later. Disable the blocks extension.
    let clang_args: Vec<String> = get_extra_args_macos();
    let mut clang_args: Vec<&str> = clang_args.iter().map(AsRef::as_ref).collect();
    clang_args.extend_from_slice(extra_clang_args);

    let mut top_level_ccfg = None;
    let mut workspace_members = vec![];
    let mut num_transpiled_files = 0;
    let mut transpiled_modules = Vec::new();
    let build_dir = get_build_dir(&tcfg, cc_db);
    for lcmd in &lcmds {
        let cmds = &lcmd.cmd_inputs;
        let lcmd_name = lcmd
            .output
            .as_ref()
            .map(|output| {
                let output_path = Path::new(output);
                output_path
                    .file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .unwrap_or_else(|| tcfg.crate_name());
        let build_dir = if lcmd.top_level {
            build_dir.to_path_buf()
        } else {
            build_dir.join(&lcmd_name)
        };

        // Compute the common ancestor of all input files
        // FIXME: this is quadratic-time in the length of the ancestor path
        let mut ancestor_path = cmds
            .first()
            .map(|cmd| {
                let mut dir = cmd.abs_file();
                dir.pop(); // discard the file part
                dir
            })
            .unwrap_or_else(PathBuf::new);
        if cmds.len() > 1 {
            for cmd in &cmds[1..] {
                let cmd_path = cmd.abs_file();
                ancestor_path = ancestor_path
                    .ancestors()
                    .find(|a| cmd_path.starts_with(a))
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(PathBuf::new);
            }
        }

        let pre_results = cmds
            .iter()
            .filter(|cmd| {
                !tcfg.is_binary(
                    &(dependency_graph
                        .nodes
                        .iter()
                        .find(|dep| dep.input_path == cmd.abs_file().to_str().unwrap())
                        .unwrap()),
                )
            })
            .map(|cmd| {
                transpile_single(
                    &tcfg,
                    cmd.abs_file(),
                    cmd.abs_output_file(),
                    &ancestor_path,
                    &build_dir,
                    cc_db,
                    &clang_args,
                    &dependency_graph,
                    |_, _| "".to_string(),
                )
            })
            .collect::<Vec<_>>();
        let results = cmds
            .iter()
            .filter(|cmd| {
                tcfg.is_binary(
                    &(dependency_graph
                        .nodes
                        .iter()
                        .find(|dep| dep.input_path == cmd.abs_file().to_str().unwrap())
                        .unwrap()),
                )
            })
            .map(|cmd| {
                let mut modules = vec![];
                let mut pragmas = PragmaSet::new();
                let mut crates = CrateSet::new();
                println!(
                    "Getting sub dependency graph for {:?}",
                    (
                        Some(cmd.abs_file().to_str().unwrap_or_default().to_string()),
                        cmd.abs_output_file()
                            .as_ref()
                            .map(|path| path.to_str().unwrap_or_default().to_string()),
                    )
                );
                let sub_dependency_graph = if let Some(idx) = dependency_graph
                    .get_node_index_with_input(
                        &cmd.abs_file().to_str().unwrap().to_string(),
                        &cmd.abs_output_file()
                            .as_ref()
                            .map(|path| path.to_str().unwrap().to_string()),
                    ) {
                    println!("Extracting sub dependency graph for {:?}", idx);
                    dependency_graph.extract_sub_dependency(vec![idx])
                } else {
                    DependencyGraph::new()
                };
                for res in &pre_results {
                    match res {
                        Ok((module, pragma_vec, crate_set)) => {
                            if let Some(_) = sub_dependency_graph
                                .get_node_index_with_output(&module.to_str().unwrap().to_string())
                            {
                                modules.push(module.clone());
                                crates.extend(crate_set);

                                for (key, vals) in pragma_vec {
                                    for val in vals {
                                        pragmas.insert((key, val));
                                    }
                                }
                            }
                        }
                        Err(_) => {}
                    }
                }
                transpile_single(
                    &tcfg,
                    cmd.abs_file(),
                    cmd.abs_output_file(),
                    &ancestor_path,
                    &build_dir,
                    cc_db,
                    &clang_args,
                    &dependency_graph,
                    |pragma_vec, crate_set| {
                        crates.extend(crate_set);
                        for (key, vals) in pragma_vec {
                            for val in vals {
                                pragmas.insert((key, val));
                            }
                        }
                        pragmas.sort();
                        crates.sort();
                        get_lib(
                            &tcfg,
                            &build_dir,
                            modules,
                            pragmas,
                            &crates,
                            &dependency_graph,
                        )
                    },
                )
            })
            .collect::<Vec<_>>()
            .into_iter()
            .chain(pre_results.into_iter())
            .collect::<Vec<_>>();
        let mut modules = vec![];
        let mut modules_skipped = false;
        let mut pragmas = PragmaSet::new();
        let mut crates = CrateSet::new();
        for res in results {
            match res {
                Ok((module, pragma_vec, crate_set)) => {
                    modules.push(module);
                    crates.extend(crate_set);

                    num_transpiled_files += 1;
                    for (key, vals) in pragma_vec {
                        for val in vals {
                            pragmas.insert((key, val));
                        }
                    }
                }
                Err(_) => {
                    modules_skipped = true;
                }
            }
        }
        pragmas.sort();
        crates.sort();

        transpiled_modules.extend(modules.iter().cloned());

        if tcfg.emit_build_files {
            if modules_skipped {
                // If we skipped a file, we may not have collected all required pragmas
                warn!("Can't emit build files after incremental transpiler run; skipped.");
                return;
            }

            let ccfg = CrateConfig {
                crate_name: lcmd_name.clone(),
                modules,
                pragmas,
                crates,
                link_cmd: lcmd,
            };
            if lcmd.top_level {
                top_level_ccfg = Some(ccfg);
            } else {
                let crate_file =
                    emit_build_files(&tcfg, &build_dir, Some(ccfg), None, &dependency_graph);
                reorganize_definitions(&tcfg, &build_dir, crate_file)
                    .unwrap_or_else(|e| warn!("Reorganizing definitions failed: {}", e));
                workspace_members.push(lcmd_name);
            }
        }
    }

    if num_transpiled_files == 0 {
        warn!("No C files found in compile_commands.json; nothing to do.");
        return;
    }

    if tcfg.emit_build_files {
        let crate_file = emit_build_files(
            &tcfg,
            &build_dir,
            top_level_ccfg,
            Some(workspace_members),
            &dependency_graph,
        );
        reorganize_definitions(&tcfg, &build_dir, crate_file)
            .unwrap_or_else(|e| warn!("Reorganizing definitions failed: {}", e));
    }

    tcfg.check_if_all_binaries_used(&transpiled_modules);
}

/// Before translate is called, exporter gens deps info
/// clap::App::get_matches().
pub fn export(
    tcfg: TranspilerConfig,
    cc_db: &Path,
    extra_clang_args: &[&str],
) -> Vec<DependencyInfo> {
    diagnostics::init(tcfg.enabled_warnings.clone(), tcfg.log_level);

    let lcmds = get_compile_commands(cc_db, &tcfg.filter).unwrap_or_else(|_| {
        panic!(
            "Could not parse compile commands from {}",
            cc_db.to_string_lossy()
        )
    });

    let mut dependency_infos = Vec::<DependencyInfo>::new();

    // Specify path to system include dir on macOS 10.14 and later. Disable the blocks extension.
    let clang_args: Vec<String> = get_extra_args_macos();
    let mut clang_args: Vec<&str> = clang_args.iter().map(AsRef::as_ref).collect();
    clang_args.extend_from_slice(extra_clang_args);

    let mut num_transpiled_files = 0;
    let build_dir = get_build_dir_raw(&tcfg, cc_db);
    for lcmd in &lcmds {
        let cmds = &lcmd.cmd_inputs;
        let lcmd_name = lcmd
            .output
            .as_ref()
            .map(|output| {
                let output_path = Path::new(output);
                output_path
                    .file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .unwrap_or_else(|| tcfg.crate_name());
        let build_dir = if lcmd.top_level {
            build_dir.to_path_buf()
        } else {
            build_dir.join(&lcmd_name)
        };

        // Compute the common ancestor of all input files
        // FIXME: this is quadratic-time in the length of the ancestor path
        let mut ancestor_path = cmds
            .first()
            .map(|cmd| {
                let mut dir = cmd.abs_file();
                dir.pop(); // discard the file part
                dir
            })
            .unwrap_or_else(PathBuf::new);
        if cmds.len() > 1 {
            for cmd in &cmds[1..] {
                let cmd_path = cmd.abs_file();
                ancestor_path = ancestor_path
                    .ancestors()
                    .find(|a| cmd_path.starts_with(a))
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(PathBuf::new);
            }
        }
        let results = cmds
            .iter()
            .map(|cmd| {
                export_single(
                    &tcfg,
                    cmd.abs_file(),
                    cmd.abs_output_file(),
                    &ancestor_path,
                    &build_dir,
                    cc_db,
                    &clang_args,
                )
            })
            .collect::<Vec<Result<DependencyInfo, ()>>>();

        // add all dependencies from results to the dependency_infos
        for res in &results {
            match res {
                Ok(_) => {
                    num_transpiled_files += 1;
                }
                Err(_) => {}
            }
        }
        dependency_infos.extend(results.into_iter().filter_map(|res| res.ok()));
    }

    if num_transpiled_files == 0 {
        warn!("No C files found in compile_commands.json; nothing to do.");
        return dependency_infos;
    }

    let mut dep_file = match File::create(&tcfg.dependency_file) {
        Ok(file) => file,
        Err(e) => panic!(
            "Unable to open file {} for writing: {}",
            tcfg.dependency_file.display(),
            e
        ),
    };

    println!(
        "Writing dependencies to file {}",
        tcfg.dependency_file.display()
    );

    match dep_file.write_all(serde_json::to_string(&dependency_infos).unwrap().as_bytes()) {
        Ok(()) => (),
        Err(e) => panic!(
            "Unable to write dependencies to file {}: {}",
            tcfg.dependency_file.display(),
            e
        ),
    };

    dependency_infos
}

/// Ensure that clang can locate the system headers on macOS 10.14+.
///
/// MacOS 10.14 does not have a `/usr/include` folder even if Xcode
/// or the command line developer tools are installed as explained in
/// this [thread](https://forums.developer.apple.com/thread/104296).
/// It is possible to install a package which puts the headers in
/// `/usr/include` but the user doesn't have to since we can find
/// the system headers we need by running `xcrun --show-sdk-path`.
fn get_extra_args_macos() -> Vec<String> {
    let mut args = vec![];
    if cfg!(target_os = "macos") {
        let usr_incl = Path::new("/usr/include");
        if !usr_incl.exists() {
            let output = process::Command::new("xcrun")
                .args(&["--show-sdk-path"])
                .output()
                .expect("failed to run `xcrun` subcommand");
            let mut sdk_path = String::from_utf8(output.stdout).unwrap();
            let olen = sdk_path.len();
            sdk_path.truncate(olen - 1);
            sdk_path.push_str("/usr/include");

            args.push("-isystem".to_owned());
            args.push(sdk_path);
        }

        // disable Apple's blocks extension; see https://github.com/immunant/c2rust/issues/229
        args.push("-fno-blocks".to_owned());
    }
    args
}

fn invoke_refactor(_build_dir: &Path) -> Result<(), Error> {
    Ok(())
}

fn reorganize_definitions(
    tcfg: &TranspilerConfig,
    build_dir: &Path,
    crate_file: Option<PathBuf>,
) -> Result<(), Error> {
    // We only run the reorganization refactoring if we emitted a fresh crate file
    if crate_file.is_none() || tcfg.disable_refactoring || !tcfg.reorganize_definitions {
        return Ok(());
    }

    invoke_refactor(build_dir)?;
    // fix the formatting of the output of `c2rust-refactor`
    let status = process::Command::new("cargo")
        .args(&["fmt"])
        .current_dir(build_dir)
        .status()?;
    if !status.success() {
        warn!("cargo fmt failed, code may not be well-formatted");
    }
    Ok(())
}

fn transpile_single(
    tcfg: &TranspilerConfig,
    input_path: PathBuf,
    output_path: Option<PathBuf>,
    ancestor_path: &Path,
    build_dir: &Path,
    cc_db: &Path,
    extra_clang_args: &[&str],
    dependency_graph: &DependencyGraph,
    get_prefix: impl FnOnce(&PragmaVec, &CrateSet) -> String,
) -> TranspileResult {
    let output_path = get_output_path(
        tcfg,
        input_path.clone(),
        output_path,
        ancestor_path,
        build_dir,
        tcfg.is_binary(
            &(dependency_graph
                .nodes
                .iter()
                .find(|dep| dep.input_path == input_path.to_str().unwrap())
                .unwrap()),
        ),
    );
    if output_path.exists() && !tcfg.overwrite_existing {
        warn!("Skipping existing file {}", output_path.display());
        return Err(());
    }

    let file = input_path.file_name().unwrap().to_str().unwrap();
    if !input_path.exists() {
        warn!(
            "Input C file {} does not exist, skipping!",
            input_path.display()
        );
        return Err(());
    }

    if tcfg.verbose {
        println!("Additional Clang arguments: {}", extra_clang_args.join(" "));
    }

    // Extract the untyped AST from the CBOR file
    let untyped_context = match ast_exporter::get_untyped_ast(
        input_path.as_path(),
        cc_db,
        extra_clang_args,
        tcfg.debug_ast_exporter,
    ) {
        Err(e) => {
            warn!(
                "Error: {}. Skipping {}; is it well-formed C?",
                e,
                input_path.display()
            );
            return Err(());
        }
        Ok(cxt) => cxt,
    };

    println!("Transpiling {}", file);

    if tcfg.dump_untyped_context {
        println!("CBOR Clang AST");
        println!("{:#?}", untyped_context);
    }

    // Convert this into a typed AST
    let typed_context = {
        let conv = ConversionContext::new(&untyped_context);
        if conv.invalid_clang_ast && tcfg.fail_on_error {
            panic!("Clang AST was invalid");
        }
        conv.typed_context
    };

    if tcfg.dump_typed_context {
        println!("Clang AST");
        println!("{:#?}", typed_context);
    }

    if tcfg.pretty_typed_context {
        println!("Pretty-printed Clang AST");
        println!("{:#?}", Printer::new(io::stdout()).print(&typed_context));
    }

    // Perform the translation
    let (mut translated_string, pragmas, crates) = translator::translate(
        typed_context,
        tcfg,
        &input_path,
        tcfg.is_binary(
            &(dependency_graph
                .nodes
                .iter()
                .find(|dep| dep.input_path == input_path.to_str().unwrap())
                .unwrap()),
        ),
    );

    if tcfg.emit_binaries
        && tcfg.is_binary(
            &(dependency_graph
                .nodes
                .iter()
                .find(|dep| dep.input_path == input_path.to_str().unwrap())
                .unwrap()),
        )
    {
        translated_string = get_prefix(&pragmas, &crates) + &translated_string;
    }

    let mut file = match File::create(&output_path) {
        Ok(file) => file,
        Err(e) => panic!(
            "Unable to open file {} for writing: {}",
            output_path.display(),
            e
        ),
    };

    match file.write_all(translated_string.as_bytes()) {
        Ok(()) => (),
        Err(e) => panic!(
            "Unable to write translation to file {}: {}",
            output_path.display(),
            e
        ),
    };

    Ok((output_path, pragmas, crates))
}

fn export_single(
    tcfg: &TranspilerConfig,
    input_path: PathBuf,
    output_path: Option<PathBuf>,
    ancestor_path: &Path,
    build_dir: &Path,
    cc_db: &Path,
    extra_clang_args: &[&str],
) -> Result<DependencyInfo, ()> {
    let raw_output_path = get_output_path_raw(
        tcfg,
        input_path.clone(),
        &output_path,
        ancestor_path,
        build_dir,
    );

    let file = input_path.file_name().unwrap().to_str().unwrap();
    if !input_path.exists() {
        warn!(
            "Input C file {} does not exist, skipping!",
            input_path.display()
        );
        return Err(());
    }

    if tcfg.verbose {
        println!("Additional Clang arguments: {}", extra_clang_args.join(" "));
    }

    // Extract the untyped AST from the CBOR file
    let untyped_context = match ast_exporter::get_untyped_ast(
        input_path.as_path(),
        cc_db,
        extra_clang_args,
        tcfg.debug_ast_exporter,
    ) {
        Err(e) => {
            warn!(
                "Error: {}. Skipping {}; is it well-formed C?",
                e,
                input_path.display()
            );
            return Err(());
        }
        Ok(cxt) => cxt,
    };

    println!("Exporting {}", file);

    if tcfg.dump_untyped_context {
        println!("CBOR Clang AST");
        println!("{:#?}", untyped_context);
    }

    // Convert this into a typed AST
    let typed_context = {
        let conv = ConversionContext::new(&untyped_context);
        if conv.invalid_clang_ast && tcfg.fail_on_error {
            panic!("Clang AST was invalid");
        }
        conv.typed_context
    };

    if tcfg.dump_typed_context {
        println!("Clang AST");
        println!("{:#?}", typed_context);
    }

    if tcfg.pretty_typed_context {
        println!("Pretty-printed Clang AST");
        println!("{:#?}", Printer::new(io::stdout()).print(&typed_context));
    }

    let mut export_context = typed_context.clone();

    export_context.prune_unwanted_decls(tcfg.preserve_unused_functions);

    let mut dependency_info = DependencyInfo {
        input_path: input_path.to_str().unwrap().to_string(),
        output_path: raw_output_path.to_str().unwrap().to_string(),
        object_path: output_path
            .clone()
            .map(|path| path.to_str().unwrap().to_string()),
        undefined: vec![],
        defined: vec![],
    };

    for (_, decl) in export_context.iter_decls() {
        match &decl.kind {
            CDeclKind::Function {
                is_global: true,
                name,
                body,
                ..
            } => {
                let decl_file = export_context
                    .get_file_path(export_context.file_id(decl).unwrap())
                    .unwrap();
                // println!(
                //     "Function: {}, is_global: {}, is_implicit: {}, is_extern: {}, body: {}",
                //     name,
                //     is_global,
                //     is_implicit,
                //     is_extern,
                //     body.is_some()
                // );

                if !body.is_some() {
                    println!("U {}", name);
                    dependency_info.undefined.push(DependencySymbol {
                        name: name.to_string(),
                        path: decl_file.to_str().unwrap().to_string(),
                    });
                } else if body.is_some() {
                    println!("T {}", name);
                    dependency_info.defined.push(DependencySymbol {
                        name: name.to_string(),
                        path: decl_file.to_str().unwrap().to_string(),
                    });
                } else {
                    assert!(false);
                }
            }
            CDeclKind::Variable {
                is_externally_visible: true,
                is_defn,
                ident,
                ..
            } => {
                let decl_file = export_context
                    .get_file_path(export_context.file_id(decl).unwrap())
                    .unwrap();

                // println!(
                //     "Variable: {:?}, is_defn: {}, is_externally_visible: {}",
                //     name, is_defn, true
                // );
                if *is_defn {
                    println!("b {}", ident);
                    dependency_info.defined.push(DependencySymbol {
                        name: ident.to_string(),
                        path: decl_file.to_str().unwrap().to_string(),
                    });
                } else {
                    println!("U {}", ident);
                    dependency_info.undefined.push(DependencySymbol {
                        name: ident.to_string(),
                        path: decl_file.to_str().unwrap().to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    let output_path = get_output_path(
        tcfg,
        input_path.clone(),
        output_path,
        ancestor_path,
        build_dir,
        tcfg.is_binary(&dependency_info),
    );

    dependency_info.output_path = output_path.to_str().unwrap().to_string();

    Ok(dependency_info)
}

fn get_output_path(
    tcfg: &TranspilerConfig,
    mut input_path: PathBuf,
    output_path: Option<PathBuf>,
    ancestor_path: &Path,
    build_dir: &Path,
    is_binary: bool,
) -> PathBuf {
    // When an output file name is not explictly specified, we should convert files
    // with dashes to underscores, as they are not allowed in rust file names.
    if let Some(output_path) = output_path {
        input_path = output_path;
    }
    let file_name = input_path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .replace('-', "_");

    input_path.set_file_name(file_name);
    input_path.set_extension("rs");

    if tcfg.output_dir.is_some() {
        let path_buf = input_path
            .strip_prefix(ancestor_path)
            .expect("Couldn't strip common ancestor path");

        // Place the source files in build_dir/src/
        let mut output_path = build_dir.to_path_buf();
        if is_binary {
            let elem = path_buf.iter().last().unwrap();
            let path = Path::new(elem);
            let name = get_module_name(path, false, true, false).unwrap();
            output_path.push(name);
        } else {
            output_path.push("src");
            for elem in path_buf.iter() {
                let path = Path::new(elem);
                let name = get_module_name(path, false, true, false).unwrap();
                output_path.push(name);
            }
        }

        // Create the parent directory if it doesn't exist
        let parent = output_path.parent().unwrap();
        if !parent.exists() {
            fs::create_dir_all(&parent).unwrap_or_else(|_| {
                panic!("couldn't create source directory: {}", parent.display())
            });
        }
        output_path
    } else {
        input_path
    }
}

fn get_output_path_raw(
    tcfg: &TranspilerConfig,
    mut input_path: PathBuf,
    output_path: &Option<PathBuf>,
    ancestor_path: &Path,
    build_dir: &Path,
) -> PathBuf {
    // When an output file name is not explictly specified, we should convert files
    // with dashes to underscores, as they are not allowed in rust file names.
    if let Some(output_path) = output_path {
        input_path = output_path.clone();
    }
    let file_name = input_path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .replace('-', "_");

    input_path.set_file_name(file_name);
    input_path.set_extension("rs");

    if tcfg.output_dir.is_some() {
        let path_buf = input_path
            .strip_prefix(ancestor_path)
            .expect("Couldn't strip common ancestor path");

        // Place the source files in build_dir/src/
        let mut output_path = build_dir.to_path_buf();
        output_path.push("src");
        for elem in path_buf.iter() {
            let path = Path::new(elem);
            let name = get_module_name(path, false, true, false).unwrap();
            output_path.push(name);
        }

        output_path
    } else {
        input_path
    }
}
