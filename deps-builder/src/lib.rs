use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::option;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DependencySymbol {
    pub name: String,
    pub path: String,
}

impl DependencySymbol {
    pub fn depends_on(&self, other: &Self, fuzz_depends_level: usize) -> bool {
        match fuzz_depends_level {
            0 => self == other,
            1 => {
                self.name == other.name
                    && Path::new(&self.path).parent() == Path::new(&other.path).parent()
                    && Path::new(&self.path).file_stem() == Path::new(&other.path).file_stem()
            }
            2 => {
                self.name == other.name
                    && Path::new(&self.path).parent() == Path::new(&other.path).parent()
            }
            3 => self.name == other.name,
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyInfo {
    pub input_path: String,
    pub output_path: String,
    pub object_path: Option<String>,
    pub undefined: Vec<DependencySymbol>,
    pub defined: Vec<DependencySymbol>,
}

impl PartialEq for DependencyInfo {
    fn eq(&self, other: &Self) -> bool {
        self.input_path == other.input_path && self.output_path == other.output_path
    }
}

impl DependencyInfo {
    pub fn is_main(&self) -> bool {
        self.defined.iter().any(|s| s.name == "main")
    }
}

#[derive(Debug)]
pub struct DependencyGraph {
    pub nodes: Vec<DependencyInfo>,
    pub edges: Vec<Vec<usize>>,
}

impl DependencyGraph {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    pub fn add_node(&mut self, node: DependencyInfo) {
        self.nodes.push(node);
        self.edges.push(Vec::new());
    }

    pub fn add_edge(&mut self, from: usize, to: usize) {
        assert!(from < self.nodes.len());
        assert!(to < self.nodes.len());
        self.edges[from].push(to);
    }

    pub fn build_dependency_edges(&mut self, fuzz_depends_level: usize) {
        for (i, node) in self.nodes.iter().enumerate() {
            for symbol in &node.undefined {
                self.nodes.iter().enumerate().for_each(|(j, n)| {
                    if !n.is_main()
                        && n.defined
                            .iter()
                            .any(|s| s.depends_on(symbol, fuzz_depends_level))
                    {
                        self.edges[i].push(j);
                    }
                });
            }
        }
    }

    pub fn get_node_index_with_input(
        &self,
        input_path: &String,
        object_path: &Option<String>,
    ) -> Option<usize> {
        self.nodes
            .iter()
            .position(|node| node.input_path == *input_path && node.object_path == *object_path)
    }

    pub fn get_node_index_with_output(&self, output_path: &String) -> Option<usize> {
        self.nodes
            .iter()
            .position(|node| node.output_path == *output_path)
    }

    pub fn direct_depends_on(&self, from: usize, to: usize) -> bool {
        self.edges[from].contains(&to)
    }

    pub fn depends_on(&self, from: usize, to: usize) -> bool {
        let mut visited = vec![false; self.nodes.len()];
        let mut queue = vec![from];

        while !queue.is_empty() {
            let current_node_index = queue.pop().unwrap();
            if visited[current_node_index] {
                continue;
            }

            visited[current_node_index] = true;
            if current_node_index == to {
                return true;
            }

            for &next_node_index in &self.edges[current_node_index] {
                queue.push(next_node_index);
            }
        }

        false
    }

    pub fn build_sub_graph(&self, nodes: &Vec<usize>) -> DependencyGraph {
        let mut sub_dependency_graph = DependencyGraph::new();

        for &node_index in nodes {
            sub_dependency_graph.add_node(self.nodes[node_index].clone());
        }

        for (i, &node_index) in nodes.iter().enumerate() {
            for &next_node_index in &self.edges[node_index] {
                if nodes.contains(&next_node_index) {
                    sub_dependency_graph
                        .add_edge(i, nodes.iter().position(|&x| x == next_node_index).unwrap());
                }
            }
        }

        sub_dependency_graph
    }

    pub fn extract_sub_dependency(&self, nodes: Vec<usize>) -> DependencyGraph {
        let mut all_nodes = vec![];

        let mut visited = vec![false; self.nodes.len()];
        let mut queue = nodes;

        while !queue.is_empty() {
            let current_node_index = queue.pop().unwrap();
            if visited[current_node_index] {
                continue;
            }

            visited[current_node_index] = true;
            all_nodes.push(current_node_index);

            for &next_node_index in &self.edges[current_node_index] {
                queue.push(next_node_index);
            }
        }

        self.build_sub_graph(&all_nodes)
    }
}

pub fn read_dependencies(dependency_file: &Path) -> Result<Vec<DependencyInfo>, Box<dyn Error>> {
    let file = File::open(dependency_file)?;
    let reader = BufReader::new(file);
    let dependencies: Vec<DependencyInfo> = serde_json::from_reader(reader)?;
    Ok(dependencies)
}

pub fn build_dependency(
    dependency_infos: Vec<DependencyInfo>,
    fuzz_depends_level: usize,
) -> DependencyGraph {
    let mut dependency_graph = DependencyGraph::new();

    dependency_infos.into_iter().for_each(|dependency| {
        dependency_graph.add_node(dependency);
    });

    dependency_graph.build_dependency_edges(fuzz_depends_level);

    dependency_graph
}
