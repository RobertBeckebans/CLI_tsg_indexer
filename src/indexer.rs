use anyhow::{Context, Result};
use log::{debug, warn};
use stack_graphs::arena::Handle;
use stack_graphs::graph::{File, StackGraph};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use streaming_iterator::StreamingIterator;
use tree_sitter as ts;

use crate::languages::Language;

pub struct SymbolEntry {
    pub name: String,
    pub kind: String,
    pub signature: Option<String>,
    pub doc: Option<String>,
}

pub type SymbolIndex = BTreeMap<String, Vec<SymbolEntry>>;

pub fn record_symbol(symbols: &mut SymbolIndex, file_name: &str, kind: &str, name: &str) {
    symbols
        .entry(file_name.to_string())
        .or_default()
        .push(SymbolEntry {
            name: name.to_string(),
            kind: kind.to_string(),
            signature: None,
            doc: None,
        });
}

pub fn record_symbol_with_details(
    symbols: &mut SymbolIndex,
    file_name: &str,
    kind: &str,
    name: &str,
    signature: Option<String>,
    doc: Option<String>,
) {
    symbols
        .entry(file_name.to_string())
        .or_default()
        .push(SymbolEntry {
            name: name.to_string(),
            kind: kind.to_string(),
            signature,
            doc,
        });
}

fn find_signature_node(node: ts::Node) -> ts::Node {
    let mut current = node;
    let mut candidate = node;
    while let Some(parent) = current.parent() {
        let kind = parent.kind();
        if kind == "template_declaration" {
            candidate = parent;
            break;
        }
        if kind == "function_definition" || kind == "declaration" || kind == "field_declaration" {
            candidate = parent;
        }
        current = parent;
    }
    candidate
}

fn signature_text_from_node(source: &str, node: ts::Node) -> Option<String> {
    let start = node.start_byte();
    let end = node.end_byte();
    let text = source.get(start..end)?;
    let trimmed = if let Some(idx) = text.find('{') {
        text[..idx].trim()
    } else {
        text.trim()
    };
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn extract_leading_comment(source: &str, start_row: usize) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut row = start_row;
    while row > 0 && lines[row - 1].trim().is_empty() {
        row -= 1;
    }
    if row == 0 {
        return None;
    }

    let line = lines[row - 1].trim();
    let mut collected: Vec<String> = Vec::new();

    let is_line_comment =
        line.starts_with("///") || line.starts_with("//!") || line.starts_with("//");
    if is_line_comment {
        let mut start = row - 1;
        while start > 0 {
            let prev = lines[start - 1].trim();
            if prev.starts_with("///") || prev.starts_with("//!") || prev.starts_with("//") {
                start -= 1;
                continue;
            }
            break;
        }
        for i in start..=row - 1 {
            let trimmed = lines[i].trim();
            let cleaned = if trimmed.starts_with("///") {
                trimmed.trim_start_matches("///").trim()
            } else if trimmed.starts_with("//!") {
                trimmed.trim_start_matches("//!").trim()
            } else {
                trimmed.trim_start_matches("//").trim()
            };
            collected.push(cleaned.to_string());
        }
    } else if line.ends_with("*/")
        || line.starts_with("/*")
        || line.starts_with("/**")
        || line.starts_with("/*!")
        || line.starts_with("*")
    {
        let mut start = row - 1;
        while start > 0 {
            let raw = lines[start].trim();
            if raw.contains("/*") || raw.contains("/**") || raw.contains("/*!") {
                break;
            }
            start -= 1;
        }
        for i in start..=row - 1 {
            let raw = lines[i].trim();
            let cleaned = raw
                .trim_start_matches("/**")
                .trim_start_matches("/*!")
                .trim_start_matches("/*")
                .trim_end_matches("*/")
                .trim_start_matches('*')
                .trim()
                .to_string();
            collected.push(cleaned);
        }
    }

    if collected.iter().all(|s| s.is_empty()) {
        None
    } else {
        Some(collected.join("\n"))
    }
}

fn find_enclosing_type_name(node: ts::Node, source: &str) -> Option<String> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        let kind = parent.kind();
        if kind == "class_specifier" || kind == "struct_specifier" {
            let mut i = 0;
            while i < parent.child_count() {
                if let Some(child) = parent.child(i) {
                    if child.kind() == "type_identifier" {
                        return source
                            .get(child.start_byte()..child.end_byte())
                            .map(|s| s.to_string());
                    }
                }
                i += 1;
            }
        }
        current = parent;
    }
    None
}

fn qualify_signature(
    signature: Option<String>,
    name: &str,
    qualified_name: &str,
) -> Option<String> {
    let sig = signature?;
    if sig.contains("::") {
        return Some(sig);
    }
    Some(sig.replacen(name, qualified_name, 1))
}

/// Index a single file and add its contents to the stack graph database
pub fn index_file(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    path: &Path,
) -> Result<()> {
    debug!("Indexing file: {}", path.display());

    // Get file extension
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");

    // Determine language
    let language = Language::from_extension(extension);

    // Skip unknown languages
    if matches!(language, Language::Unknown) {
        warn!(
            "Skipping file with unsupported extension: {}",
            path.display()
        );
        return Ok(());
    }

    // Read file content (lossy UTF-8 to tolerate legacy encodings)
    let bytes =
        fs::read(path).with_context(|| format!("Failed to read file: {}", path.display()))?;
    let content = String::from_utf8_lossy(&bytes).to_string();

    // Get language parser
    let mut parser = match language.get_parser() {
        Some(parser) => parser,
        None => {
            warn!("No parser available for language: {}", language.name());
            return Ok(());
        }
    };

    // Parse the file
    let tree = parser
        .parse(&content, None)
        .with_context(|| format!("Failed to parse file: {}", path.display()))?;

    // Process the syntax tree with tree-sitter-stack-graphs
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Use files API to add to the database
    process_syntax_tree(stack_graph, symbols, &language, &file_name, &tree, &content)?;

    debug!("Successfully indexed file: {}", path.display());
    Ok(())
}

/// Recursively index a directory and add its contents to the stack graph
pub fn index_directory(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    dir: &Path,
) -> Result<()> {
    debug!("Indexing directory: {}", dir.display());

    for entry in
        fs::read_dir(dir).with_context(|| format!("Failed to read directory: {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            // Skip files we don't want to process
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                // Expanded list of supported extensions
                let supported_extensions = [
                    "rs", "py", "js", "jsx", "ts", "tsx", "java", "swift", "m", "mm", "css",
                    "scala", "zig", "yaml", "yml", "go", "php", "xml", "sh", "bash", "json",
                    "html", "htm", "cs", "rb", "md", "markdown", "lua", "dart", "c", "cpp", "h",
                ];

                if supported_extensions.contains(&ext.to_lowercase().as_str()) {
                    index_file(stack_graph, symbols, &path)?;
                }
            }
        } else if path.is_dir() {
            // Skip hidden directories and typical build directories
            if let Some(dir_name) = path.file_name() {
                let dir_name = dir_name.to_string_lossy();
                if !dir_name.starts_with(".")
                    && ![
                        "node_modules",
                        "target",
                        "build",
                        "__pycache__",
                        "dist",
                        "venv",
                        ".git",
                        ".svn",
                        ".hg",
                        "bin",
                        "obj",
                    ]
                    .contains(&dir_name.as_ref())
                {
                    index_directory(stack_graph, symbols, &path)?;
                }
            }
        }
    }

    Ok(())
}

/// Process a syntax tree and add nodes to the stack graph
pub fn process_syntax_tree(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    language: &Language,
    file_name: &str,
    tree: &ts::Tree,
    source: &str,
) -> Result<()> {
    debug!("Processing syntax tree for file: {}", file_name);

    // Check if we have a built-in stack graphs language
    if let Some(stack_graphs_lang) = language.get_stack_graphs_language() {
        // Use the built-in stack graphs language support when available
        debug!(
            "Using built-in stack graphs language: {}",
            stack_graphs_lang
        );

        // Create a new file in the database with the specified name
        let file_handle = stack_graph.get_or_create_file(file_name);

        match stack_graphs_lang {
            "javascript" => {
                process_javascript_syntax(
                    stack_graph,
                    symbols,
                    file_handle,
                    &tree.root_node(),
                    source,
                )?;
            }
            "python" => {
                process_python_syntax(
                    stack_graph,
                    symbols,
                    file_handle,
                    &tree.root_node(),
                    source,
                )?;
            }
            "typescript" => {
                process_typescript_syntax(
                    stack_graph,
                    symbols,
                    file_handle,
                    &tree.root_node(),
                    source,
                )?;
            }
            "java" => {
                process_java_syntax(stack_graph, symbols, file_handle, &tree.root_node(), source)?;
            }
            _ => {
                // Fallback for unknown languages
                process_generic_syntax(stack_graph, symbols, file_handle, source, language)?;
            }
        }
    } else {
        // Create a file for the stack graph
        let file_handle = stack_graph.get_or_create_file(file_name);

        // Process based on language
        match language {
            Language::Rust => {
                process_rust_syntax(stack_graph, symbols, file_handle, &tree.root_node(), source)?;
            }
            Language::Cpp => {
                process_cpp_syntax(stack_graph, symbols, file_handle, &tree.root_node(), source)
                    .with_context(|| {
                        format!("Failed to process C++ syntax for file: {}", file_name)
                    })?;
            }
            Language::Go => {
                process_go_syntax(stack_graph, symbols, file_handle, &tree.root_node(), source)?;
            }
            Language::Ruby => {
                process_ruby_syntax(stack_graph, symbols, file_handle, &tree.root_node(), source)?;
            }
            _ => {
                // Generic processing for other languages
                process_generic_syntax(stack_graph, symbols, file_handle, source, language)?;
            }
        }
    }

    Ok(())
}

fn process_rust_syntax(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    file_handle: Handle<File>,
    root_node: &ts::Node,
    source: &str,
) -> Result<()> {
    // Find function and struct definitions in Rust
    let mut cursor = ts::QueryCursor::new();
    let query = ts::Query::new(
        &tree_sitter_rust::LANGUAGE.into(),
        "(function_item name: (identifier) @function_name) @function_def
         (struct_item name: (type_identifier) @struct_name) @struct_def
         (impl_item type: (type_identifier) @impl_type) @impl_def
         (trait_item name: (type_identifier) @trait_name) @trait_def
         (mod_item name: (identifier) @mod_name) @mod_def
         (enum_item name: (type_identifier) @enum_name) @enum_def",
    )?;

    // Process matches
    let mut matches = cursor.matches(&query, *root_node, source.as_bytes());

    // Process each match using the streaming iterator pattern
    while let Some(match_) = matches.next() {
        for i in 0..match_.captures.len() {
            let capture = &match_.captures[i];
            let capture_name = &query.capture_names()[capture.index as usize];
            let node = capture.node;
            let node_text = &source[node.byte_range()];

            debug!("Found Rust {}: {}", capture_name, node_text);

            // Add to stack graph
            if [
                "function_name",
                "struct_name",
                "impl_type",
                "trait_name",
                "mod_name",
                "enum_name",
            ]
            .contains(&capture_name.as_ref())
            {
                let file_name = stack_graph[file_handle].to_string();
                record_symbol(symbols, &file_name, capture_name, node_text);

                // Create a symbol for the definition
                let symbol = stack_graph.add_symbol(node_text);

                // Create a node ID for this definition
                let node_id = stack_graph.new_node_id(file_handle);

                // Create a pop symbol node (definition)
                let pop_node = stack_graph
                    .add_pop_symbol_node(node_id, symbol, true)
                    .expect("Failed to create pop symbol node");

                // Create a scope node for this definition
                let scope_id = stack_graph.new_node_id(file_handle);
                let scope_node = stack_graph
                    .add_scope_node(scope_id, true)
                    .expect("Failed to create scope node");

                // Connect the pop node to the scope node
                stack_graph.add_edge(pop_node, scope_node, 0);

                debug!(
                    "Created stack graph nodes for Rust {}: {}",
                    capture_name, node_text
                );
            }
        }
    }

    Ok(())
}

fn process_cpp_syntax(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    file_handle: Handle<File>,
    root_node: &ts::Node,
    source: &str,
) -> Result<()> {
    let mut cursor = ts::QueryCursor::new();
    let query = ts::Query::new(
        &tree_sitter_cpp::LANGUAGE.into(),
        "(function_declarator (identifier) @function_name)
         (function_declarator (field_identifier) @method_name)
         (function_declarator (qualified_identifier (identifier) @method_name))
         (class_specifier (type_identifier) @class_name)
         (struct_specifier (type_identifier) @struct_name)
         (enum_specifier (type_identifier) @enum_name)",
    )
    .with_context(|| "Failed to build C++ tree-sitter query".to_string())?;

    let mut matches = cursor.matches(&query, *root_node, source.as_bytes());

    while let Some(match_) = matches.next() {
        for i in 0..match_.captures.len() {
            let capture = &match_.captures[i];
            let capture_name = &query.capture_names()[capture.index as usize];
            let node = capture.node;
            let node_text = &source[node.byte_range()];

            debug!("Found C++ {}: {}", capture_name, node_text);

            if [
                "function_name",
                "method_name",
                "class_name",
                "struct_name",
                "enum_name",
            ]
            .contains(&capture_name.as_ref())
            {
                let file_name = stack_graph[file_handle].to_string();
                let signature_node = find_signature_node(node);
                let mut signature = signature_text_from_node(source, signature_node);
                let doc =
                    extract_leading_comment(source, signature_node.start_position().row as usize);

                let mut display_name = node_text.to_string();
                if let Some(enclosing) = find_enclosing_type_name(node, source) {
                    if !display_name.contains("::") {
                        display_name = format!("{}::{}", enclosing, display_name);
                    }
                    signature = qualify_signature(signature, node_text, &display_name);
                }

                record_symbol_with_details(
                    symbols,
                    &file_name,
                    capture_name,
                    &display_name,
                    signature,
                    doc,
                );

                let symbol = stack_graph.add_symbol(node_text);

                let node_id = stack_graph.new_node_id(file_handle);
                let pop_node = stack_graph
                    .add_pop_symbol_node(node_id, symbol, true)
                    .expect("Failed to create pop symbol node");

                let scope_id = stack_graph.new_node_id(file_handle);
                let scope_node = stack_graph
                    .add_scope_node(scope_id, true)
                    .expect("Failed to create scope node");

                stack_graph.add_edge(pop_node, scope_node, 0);

                debug!(
                    "Created stack graph nodes for C++ {}: {}",
                    capture_name, node_text
                );
            }
        }
    }

    Ok(())
}

fn process_javascript_syntax(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    file_handle: Handle<File>,
    root_node: &ts::Node,
    source: &str,
) -> Result<()> {
    // Find function and class definitions in JavaScript
    let mut cursor = ts::QueryCursor::new();
    let query = ts::Query::new(
        &tree_sitter_javascript::LANGUAGE.into(),
        "(function_declaration name: (identifier) @function_name) @function_def
         (class_declaration name: (identifier) @class_name) @class_def
         (method_definition name: (property_identifier) @method_name) @method_def
         (lexical_declaration
           (variable_declarator name: (identifier) @const_name
             value: [(function_expression) (arrow_function)]) @const_fn_def)
         (import_statement source: (string) @import_source) @import_def",
    )?;

    // Process matches
    let mut matches = cursor.matches(&query, *root_node, source.as_bytes());

    // Process each match using the streaming iterator pattern
    while let Some(match_) = matches.next() {
        for i in 0..match_.captures.len() {
            let capture = &match_.captures[i];
            let capture_name = &query.capture_names()[capture.index as usize];
            let node = capture.node;
            let node_text = &source[node.byte_range()];

            debug!("Found JavaScript {}: {}", capture_name, node_text);

            // Add to stack graph
            if [
                "function_name",
                "class_name",
                "method_name",
                "const_name",
                "import_source",
            ]
            .contains(&capture_name.as_ref())
            {
                let file_name = stack_graph[file_handle].to_string();
                record_symbol(symbols, &file_name, capture_name, node_text);

                // Create a symbol for the definition
                let symbol = stack_graph.add_symbol(node_text);

                // Create a node ID for this definition
                let node_id = stack_graph.new_node_id(file_handle);

                // Create a pop symbol node (definition)
                let pop_node = stack_graph
                    .add_pop_symbol_node(node_id, symbol, true)
                    .expect("Failed to create pop symbol node");

                // Create a scope node for this definition
                let scope_id = stack_graph.new_node_id(file_handle);
                let scope_node = stack_graph
                    .add_scope_node(scope_id, true)
                    .expect("Failed to create scope node");

                // Connect the pop node to the scope node
                stack_graph.add_edge(pop_node, scope_node, 0);

                // For imports, create a push symbol node to reference the imported module
                if ["import_source"].contains(&capture_name.as_ref()) {
                    let push_id = stack_graph.new_node_id(file_handle);
                    let push_node = stack_graph
                        .add_push_symbol_node(push_id, symbol, true)
                        .expect("Failed to create push symbol node");

                    // Connect the scope node to the push node
                    stack_graph.add_edge(scope_node, push_node, 0);
                }

                debug!(
                    "Created stack graph nodes for JavaScript {}: {}",
                    capture_name, node_text
                );
            }
        }
    }

    Ok(())
}

fn process_typescript_syntax(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    file_handle: Handle<File>,
    _root_node: &ts::Node,
    _source: &str,
) -> Result<()> {
    // TypeScript processing - create a module node for the file
    debug!("Processing TypeScript syntax");

    // Create a symbol for the module
    let file_name = stack_graph[file_handle].to_string();
    record_symbol(symbols, &file_name, "module", "module");

    let symbol = stack_graph.add_symbol("module");

    // Create a node ID for this module
    let node_id = stack_graph.new_node_id(file_handle);

    // Create a pop symbol node (definition)
    let pop_node = stack_graph
        .add_pop_symbol_node(node_id, symbol, true)
        .expect("Failed to create pop symbol node");

    // Create a scope node for this module
    let scope_id = stack_graph.new_node_id(file_handle);
    let scope_node = stack_graph
        .add_scope_node(scope_id, true)
        .expect("Failed to create scope node");

    // Connect the pop node to the scope node
    stack_graph.add_edge(pop_node, scope_node, 0);

    debug!("Created stack graph nodes for TypeScript module");

    Ok(())
}

fn process_python_syntax(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    file_handle: Handle<File>,
    root_node: &ts::Node,
    source: &str,
) -> Result<()> {
    // Find function and class definitions in Python
    let mut cursor = ts::QueryCursor::new();
    let query = ts::Query::new(
        &tree_sitter_python::LANGUAGE.into(),
        "(function_definition name: (identifier) @function_name) @function_def
         (class_definition name: (identifier) @class_name) @class_def
         (import_statement name: (dotted_name) @import_name) @import_def
         (import_from_statement module_name: (dotted_name) @module_name) @import_from",
    )?;

    // Process matches
    let mut matches = cursor.matches(&query, *root_node, source.as_bytes());

    // Process each match using the streaming iterator pattern
    while let Some(match_) = matches.next() {
        for i in 0..match_.captures.len() {
            let capture = &match_.captures[i];
            let capture_name = &query.capture_names()[capture.index as usize];
            let node = capture.node;
            let node_text = &source[node.byte_range()];

            debug!("Found Python {}: {}", capture_name, node_text);

            // Add to stack graph
            if ["function_name", "class_name", "import_name", "module_name"]
                .contains(&capture_name.as_ref())
            {
                let file_name = stack_graph[file_handle].to_string();
                record_symbol(symbols, &file_name, capture_name, node_text);

                // Create a symbol for the definition
                let symbol = stack_graph.add_symbol(node_text);

                // Create a node ID for this definition
                let node_id = stack_graph.new_node_id(file_handle);

                // Create a pop symbol node (definition)
                let pop_node = stack_graph
                    .add_pop_symbol_node(node_id, symbol, true)
                    .expect("Failed to create pop symbol node");

                // Create a scope node for this definition
                let scope_id = stack_graph.new_node_id(file_handle);
                let scope_node = stack_graph
                    .add_scope_node(scope_id, true)
                    .expect("Failed to create scope node");

                // Connect the pop node to the scope node
                stack_graph.add_edge(pop_node, scope_node, 0);

                // For imports, create a push symbol node to reference the imported module
                if ["import_name", "module_name"].contains(&capture_name.as_ref()) {
                    let push_id = stack_graph.new_node_id(file_handle);
                    let push_node = stack_graph
                        .add_push_symbol_node(push_id, symbol, true)
                        .expect("Failed to create push symbol node");

                    // Connect the scope node to the push node
                    stack_graph.add_edge(scope_node, push_node, 0);
                }

                debug!(
                    "Created stack graph nodes for Python {}: {}",
                    capture_name, node_text
                );
            }
        }
    }

    Ok(())
}

fn process_java_syntax(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    file_handle: Handle<File>,
    _root_node: &ts::Node,
    _source: &str,
) -> Result<()> {
    // Java processing - create a module node for the file
    debug!("Processing Java syntax");

    // Create a symbol for the module
    let file_name = stack_graph[file_handle].to_string();
    record_symbol(symbols, &file_name, "module", "java_module");

    let symbol = stack_graph.add_symbol("java_module");

    // Create a node ID for this module
    let node_id = stack_graph.new_node_id(file_handle);

    // Create a pop symbol node (definition)
    let pop_node = stack_graph
        .add_pop_symbol_node(node_id, symbol, true)
        .expect("Failed to create pop symbol node");

    // Create a scope node for this module
    let scope_id = stack_graph.new_node_id(file_handle);
    let scope_node = stack_graph
        .add_scope_node(scope_id, true)
        .expect("Failed to create scope node");

    // Connect the pop node to the scope node
    stack_graph.add_edge(pop_node, scope_node, 0);

    debug!("Created stack graph nodes for Java module");

    Ok(())
}

fn process_go_syntax(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    file_handle: Handle<File>,
    root_node: &ts::Node,
    source: &str,
) -> Result<()> {
    // Find function and type definitions in Go
    let mut cursor = ts::QueryCursor::new();
    let query = ts::Query::new(
        &tree_sitter_go::LANGUAGE.into(),
        "(function_declaration name: (identifier) @function_name) @function_def
         (method_declaration name: (field_identifier) @method_name) @method_def
         (type_declaration (type_spec name: (type_identifier) @type_name)) @type_def
         (import_declaration (import_spec path: (interpreted_string_literal) @import_path)) @import_def"
    )?;

    // Process matches
    let mut matches = cursor.matches(&query, *root_node, source.as_bytes());

    // Process each match using the streaming iterator pattern
    while let Some(match_) = matches.next() {
        for i in 0..match_.captures.len() {
            let capture = &match_.captures[i];
            let capture_name = &query.capture_names()[capture.index as usize];
            let node = capture.node;
            let node_text = &source[node.byte_range()];

            debug!("Found Go {}: {}", capture_name, node_text);

            // Add to stack graph
            if ["function_name", "method_name", "type_name", "import_path"]
                .contains(&capture_name.as_ref())
            {
                let file_name = stack_graph[file_handle].to_string();
                record_symbol(symbols, &file_name, capture_name, node_text);

                // Create a symbol for the definition
                let symbol = stack_graph.add_symbol(node_text);

                // Create a node ID for this definition
                let node_id = stack_graph.new_node_id(file_handle);

                // Create a pop symbol node (definition)
                let pop_node = stack_graph
                    .add_pop_symbol_node(node_id, symbol, true)
                    .expect("Failed to create pop symbol node");

                // Create a scope node for this definition
                let scope_id = stack_graph.new_node_id(file_handle);
                let scope_node = stack_graph
                    .add_scope_node(scope_id, true)
                    .expect("Failed to create scope node");

                // Connect the pop node to the scope node
                stack_graph.add_edge(pop_node, scope_node, 0);

                // For imports, create a push symbol node to reference the imported module
                if ["import_path"].contains(&capture_name.as_ref()) {
                    let push_id = stack_graph.new_node_id(file_handle);
                    let push_node = stack_graph
                        .add_push_symbol_node(push_id, symbol, true)
                        .expect("Failed to create push symbol node");

                    // Connect the scope node to the push node
                    stack_graph.add_edge(scope_node, push_node, 0);
                }

                debug!(
                    "Created stack graph nodes for Go {}: {}",
                    capture_name, node_text
                );
            }
        }
    }

    Ok(())
}

fn process_ruby_syntax(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    file_handle: Handle<File>,
    root_node: &ts::Node,
    source: &str,
) -> Result<()> {
    // Find method and class definitions in Ruby
    let mut cursor = ts::QueryCursor::new();
    let query = ts::Query::new(
        &tree_sitter_ruby::LANGUAGE.into(),
        "(method name: (identifier) @method_name) @method_def
         (class name: (constant) @class_name) @class_def
         (module name: (constant) @module_name) @module_def
         (require call: (identifier) @require_call
           argument: (string (string_content) @require_path)) @require_def",
    )?;

    // Process matches
    let mut matches = cursor.matches(&query, *root_node, source.as_bytes());

    // Process each match using the streaming iterator pattern
    while let Some(match_) = matches.next() {
        for i in 0..match_.captures.len() {
            let capture = &match_.captures[i];
            let capture_name = &query.capture_names()[capture.index as usize];
            let node = capture.node;
            let node_text = &source[node.byte_range()];

            debug!("Found Ruby {}: {}", capture_name, node_text);

            // Add to stack graph
            if ["method_name", "class_name", "module_name", "require_path"]
                .contains(&capture_name.as_ref())
            {
                let file_name = stack_graph[file_handle].to_string();
                record_symbol(symbols, &file_name, capture_name, node_text);

                // Create a symbol for the definition
                let symbol = stack_graph.add_symbol(node_text);

                // Create a node ID for this definition
                let node_id = stack_graph.new_node_id(file_handle);

                // Create a pop symbol node (definition)
                let pop_node = stack_graph
                    .add_pop_symbol_node(node_id, symbol, true)
                    .expect("Failed to create pop symbol node");

                // Create a scope node for this definition
                let scope_id = stack_graph.new_node_id(file_handle);
                let scope_node = stack_graph
                    .add_scope_node(scope_id, true)
                    .expect("Failed to create scope node");

                // Connect the pop node to the scope node
                stack_graph.add_edge(pop_node, scope_node, 0);

                // For requires, create a push symbol node to reference the required module
                if ["require_path"].contains(&capture_name.as_ref()) {
                    let push_id = stack_graph.new_node_id(file_handle);
                    let push_node = stack_graph
                        .add_push_symbol_node(push_id, symbol, true)
                        .expect("Failed to create push symbol node");

                    // Connect the scope node to the push node
                    stack_graph.add_edge(scope_node, push_node, 0);
                }

                debug!(
                    "Created stack graph nodes for Ruby {}: {}",
                    capture_name, node_text
                );
            }
        }
    }

    Ok(())
}

fn process_generic_syntax(
    stack_graph: &mut StackGraph,
    symbols: &mut SymbolIndex,
    file_handle: Handle<File>,
    _source: &str,
    language: &Language,
) -> Result<()> {
    // Basic processing for languages without specific handlers
    debug!("Using generic processing for {}", language.name());

    // Create a symbol for the module
    let file_name = stack_graph[file_handle].to_string();
    record_symbol(symbols, &file_name, "module", "module");

    let symbol = stack_graph.add_symbol("module");

    // Create a node ID for this module
    let node_id = stack_graph.new_node_id(file_handle);

    // Create a pop symbol node (definition)
    let pop_node = stack_graph
        .add_pop_symbol_node(node_id, symbol, true)
        .expect("Failed to create pop symbol node");

    // Create a scope node for this module
    let scope_id = stack_graph.new_node_id(file_handle);
    let scope_node = stack_graph
        .add_scope_node(scope_id, true)
        .expect("Failed to create scope node");

    // Connect the pop node to the scope node
    stack_graph.add_edge(pop_node, scope_node, 0);

    debug!("Created stack graph nodes for generic module");

    Ok(())
}
