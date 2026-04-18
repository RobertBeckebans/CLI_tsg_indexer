use anyhow::Result;
use log::info;
use stack_graphs::graph::StackGraph;
use std::fs;
use std::path::Path;

use crate::indexer::SymbolIndex;

/// Output the stack graph as JSON
pub fn output_json(
    stack_graph: &StackGraph,
    symbols: &SymbolIndex,
    output_path: Option<&Path>,
) -> Result<()> {
    let symbol_count: usize = symbols.values().map(|entries| entries.len()).sum();

    let output = serde_json::json!({
        "summary": {
            "files": stack_graph.iter_files().count(),
            "symbols": symbol_count,
            "description": "Tree-sitter Stack Graph representation of indexed code"
        },
        "files": stack_graph.iter_files().map(|file_handle| {
            let name = stack_graph[file_handle].to_string();
            let file_symbols = symbols
                .get(&name)
                .map(|entries| {
                    entries.iter().map(|entry| {
                        serde_json::json!({
                            "name": entry.name.as_str(),
                            "kind": entry.kind.as_str(),
                            "signature": entry.signature.as_deref(),
                            "doc": entry.doc.as_deref(),
                        })
                    }).collect::<Vec<_>>()
                })
                .unwrap_or_default();

            serde_json::json!({
                "name": name,
                "symbols": file_symbols,
            })
        }).collect::<Vec<_>>(),
    });

    let json_str = serde_json::to_string_pretty(&output)?;

    match output_path {
        Some(path) => {
            fs::write(path, json_str)?;
            info!("Stack graph written to: {}", path.display());
        }
        None => {
            println!("{}", json_str);
        }
    }

    Ok(())
}

/// Output the stack graph as DOT format for visualization
pub fn output_dot(stack_graph: &StackGraph, output_path: Option<&Path>) -> Result<()> {
    // Create a simplified DOT graph representation
    let mut dot_graph = String::new();
    dot_graph.push_str("digraph StackGraph {\n");

    // Add file nodes
    for file_handle in stack_graph.iter_files() {
        let file_name = stack_graph[file_handle].to_string();

        // Create a node for each file
        dot_graph.push_str(&format!(
            "  file_{} [label=\"{}\"];\n",
            file_handle.as_u32(),
            file_name
        ));
    }

    dot_graph.push_str("}\n");

    match output_path {
        Some(path) => {
            fs::write(path, dot_graph)?;
            info!("Stack graph written to: {}", path.display());
        }
        None => {
            println!("{}", dot_graph);
        }
    }

    Ok(())
}
