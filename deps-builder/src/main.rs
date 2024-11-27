use clap::Parser;
use dot_generator::*;
use dot_structures::*;
use graphviz_rust::printer::{DotPrinter, PrinterContext};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

use deps_builder::{build_dependency, read_dependencies};

#[derive(Debug, Parser)]
#[clap(
name = "deps-builder",
author = "- DEPSO (DEPendable SOftware) Research Group From ISCAS",
about = "Build C dependencies for C2Rust",
long_about = None,
trailing_var_arg = true)]
struct Args {
    /// Use fuzzing dependency checking
    #[clap(long, default_value_t = 0)]
    fuzz_depends_level: usize,
    /// Path to a file to with the dependency information
    #[clap(long, default_value = "./dependencies.json")]
    dependency_file: PathBuf,
    /// Path to a file to write the dependency graph to
    #[clap(long, default_value = "./dependencies.dot")]
    dependency_dot: PathBuf,
    /// Emit Rust dependencies for the given binaries
    #[clap(long, multiple = true, number_of_values = 1)]
    bin: Vec<String>,
    /// Emit Rust dependencies for all binaries (with main defined)
    #[clap(long)]
    bins: bool,
}

fn main() {
    let args = Args::parse();
    let fuzz_depends_level = args.fuzz_depends_level;
    let dependency_file = args.dependency_file;
    let dependency_dot = args.dependency_dot;
    let emit_binaries = args.bin;
    let emit_all_binaries = args.bins;

    // Read dependencies from the dependency file
    let dependency_infos = read_dependencies(&dependency_file).unwrap_or_else(|e| {
        eprintln!(
            "Error reading dependencies from {}: {}",
            dependency_file.display(),
            e
        );
        process::exit(1);
    });

    let mut bin_nodes = Vec::new();

    if emit_all_binaries {
        for (i, node) in dependency_infos.iter().enumerate() {
            if node.defined.iter().any(|s| s.name == "main") {
                bin_nodes.push(i);
            }
        }
    } else if !emit_binaries.is_empty() {
        for bin in emit_binaries {
            for (i, node) in dependency_infos.iter().enumerate() {
                if PathBuf::from(&node.output_path)
                    .file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    == bin
                    && node.defined.iter().any(|s| s.name == "main")
                {
                    bin_nodes.push(i);
                }
            }
        }
    }

    let dependency_graph = if bin_nodes.is_empty() {
        build_dependency(dependency_infos, fuzz_depends_level)
    } else {
        build_dependency(dependency_infos, fuzz_depends_level).extract_sub_dependency(bin_nodes)
    };

    // println!("Dependency Graph: {:#?}", dependency_graph);

    // Write the dependency graph to a dot file
    let mut dependency_dot_graph = Graph::DiGraph {
        id: Id::Plain(String::from("dependency_graph")),
        strict: true,
        stmts: vec![],
    };

    for (i, node) in dependency_graph.nodes.iter().enumerate() {
        if let Some(_) = node.defined.iter().find(|s| s.name == "main") {
            dependency_dot_graph.add_stmt(Stmt::Node(
                node!(i;attr!("color", "red"), attr!("label", (format!("\"{}\"", (Path::new(&node.output_path).file_name().unwrap().to_str().unwrap()))))),
            ));
        } else {
            dependency_dot_graph.add_stmt(Stmt::Node(
                node!(i;attr!("label", (format!("\"{}\"", (Path::new(&node.output_path).file_name().unwrap().to_str().unwrap()))))),
            ));
        }
    }

    for (i, edges) in dependency_graph.edges.iter().enumerate() {
        for j in edges {
            dependency_dot_graph.add_stmt(Stmt::Edge(edge!(node_id!(i) => node_id!(j))));
        }
    }

    println!(
        "{}",
        dependency_dot_graph.print(&mut PrinterContext::default())
    );

    let mut dot_file = File::create(&dependency_dot).unwrap_or_else(|e| {
        eprintln!(
            "Error creating dependency dot file {}: {}",
            dependency_dot.display(),
            e
        );
        process::exit(1);
    });

    match dot_file.write_all(
        dependency_dot_graph
            .print(&mut PrinterContext::default())
            .as_bytes(),
    ) {
        Ok(()) => (),
        Err(e) => panic!(
            "Unable to write dependencies to file {}: {}",
            dependency_dot.display(),
            e
        ),
    };
}
