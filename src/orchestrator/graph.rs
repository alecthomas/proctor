use std::collections::{HashMap, HashSet};

/// A directed acyclic graph representing process dependencies.
/// An edge from A to B means "A depends on B" (B must be ready before A can start).
pub struct DependencyGraph {
    /// Maps process name -> set of dependency names (processes it waits for)
    dependencies: HashMap<String, HashSet<String>>,
    /// Maps process name -> set of dependent names (processes waiting for it)
    dependents: HashMap<String, HashSet<String>>,
    /// All process names in the graph
    processes: HashSet<String>,
}

impl DependencyGraph {
    /// Build a dependency graph from process definitions.
    /// Each process has a name and a list of dependencies (from `after=`).
    pub fn new<'a>(processes: impl Iterator<Item = (&'a str, &'a [String])>) -> Self {
        let mut dependencies: HashMap<String, HashSet<String>> = HashMap::new();
        let mut dependents: HashMap<String, HashSet<String>> = HashMap::new();
        let mut all_processes = HashSet::new();

        for (name, deps) in processes {
            all_processes.insert(name.to_string());
            dependencies.insert(name.to_string(), deps.iter().cloned().collect());

            for dep in deps {
                dependents.entry(dep.clone()).or_default().insert(name.to_string());
            }
        }

        // Ensure all processes have entries in both maps
        for name in &all_processes {
            dependencies.entry(name.clone()).or_default();
            dependents.entry(name.clone()).or_default();
        }

        Self {
            dependencies,
            dependents,
            processes: all_processes,
        }
    }

    /// Returns processes that have no dependencies (can start immediately).
    pub fn roots(&self) -> Vec<&str> {
        self.processes
            .iter()
            .filter(|name| self.dependencies.get(*name).map(|deps| deps.is_empty()).unwrap_or(true))
            .map(|s| s.as_str())
            .collect()
    }

    /// Returns processes that depend on the given process.
    pub fn dependents_of(&self, name: &str) -> Vec<&str> {
        self.dependents
            .get(name)
            .map(|deps| deps.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }

    /// Returns the dependencies of a process (what it waits for).
    pub fn dependencies_of(&self, name: &str) -> Vec<&str> {
        self.dependencies
            .get(name)
            .map(|deps| deps.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }

    /// Returns processes in reverse topological order (for shutdown).
    /// Processes with dependents come first, roots come last.
    pub fn reverse_topological_order(&self) -> Vec<&str> {
        let mut result = Vec::new();
        let mut visited = HashSet::new();

        fn visit<'a>(
            name: &'a str,
            graph: &'a DependencyGraph,
            visited: &mut HashSet<&'a str>,
            result: &mut Vec<&'a str>,
        ) {
            if visited.contains(name) {
                return;
            }
            visited.insert(name);

            // Visit dependencies first (they should be stopped later)
            for dep in graph.dependencies_of(name) {
                visit(dep, graph, visited, result);
            }

            result.push(name);
        }

        for name in &self.processes {
            visit(name.as_str(), self, &mut visited, &mut result);
        }

        // Reverse to get: dependents first, roots last
        result.reverse();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_graph() {
        let graph = DependencyGraph::new(std::iter::empty());
        assert!(graph.roots().is_empty());
        assert!(graph.reverse_topological_order().is_empty());
    }

    #[test]
    fn test_no_dependencies() {
        let processes = vec![("a", vec![]), ("b", vec![]), ("c", vec![])];
        let graph = DependencyGraph::new(processes.iter().map(|(n, d)| (*n, d.as_slice())));

        let mut roots = graph.roots();
        roots.sort();
        assert_eq!(roots, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_linear_chain() {
        // c depends on b, b depends on a
        let processes = vec![
            ("a", vec![]),
            ("b", vec!["a".to_string()]),
            ("c", vec!["b".to_string()]),
        ];
        let graph = DependencyGraph::new(processes.iter().map(|(n, d)| (*n, d.as_slice())));

        assert_eq!(graph.roots(), vec!["a"]);
        assert_eq!(graph.dependencies_of("c"), vec!["b"]);
        assert_eq!(graph.dependents_of("a"), vec!["b"]);
        assert_eq!(graph.dependents_of("b"), vec!["c"]);

        // Reverse topo: c, b, a (shutdown order)
        let order = graph.reverse_topological_order();
        assert_eq!(order, vec!["c", "b", "a"]);
    }

    #[test]
    fn test_diamond_dependencies() {
        //     a
        //    / \
        //   b   c
        //    \ /
        //     d
        let processes = vec![
            ("a", vec![]),
            ("b", vec!["a".to_string()]),
            ("c", vec!["a".to_string()]),
            ("d", vec!["b".to_string(), "c".to_string()]),
        ];
        let graph = DependencyGraph::new(processes.iter().map(|(n, d)| (*n, d.as_slice())));

        assert_eq!(graph.roots(), vec!["a"]);

        let mut a_dependents = graph.dependents_of("a");
        a_dependents.sort();
        assert_eq!(a_dependents, vec!["b", "c"]);

        // d should be first in shutdown order
        let order = graph.reverse_topological_order();
        assert_eq!(order[0], "d");
        assert_eq!(*order.last().unwrap(), "a");
    }

    #[test]
    fn test_multiple_roots() {
        // a and b are roots, c depends on both
        let processes = vec![
            ("a", vec![]),
            ("b", vec![]),
            ("c", vec!["a".to_string(), "b".to_string()]),
        ];
        let graph = DependencyGraph::new(processes.iter().map(|(n, d)| (*n, d.as_slice())));

        let mut roots = graph.roots();
        roots.sort();
        assert_eq!(roots, vec!["a", "b"]);

        // c must be first in shutdown order
        let order = graph.reverse_topological_order();
        assert_eq!(order[0], "c");
    }
}
