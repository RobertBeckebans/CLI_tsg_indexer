use env_logger;
use std::path::PathBuf;
use tsg_indexer::{IndexerConfig, run_indexer};

fn main() {
    // Initialize the logger
    env_logger::init();

    // Create configuration for the indexer
    let config = IndexerConfig {
        // Index the examples directory with sample files in different languages
        path: PathBuf::from("examples"),

        // Output in JSON format
        format: "json".to_string(),

        // Print to stdout
        output: Some(PathBuf::from("graph.json")),

        // Enable verbose logging
        verbose: true,

        // Enable automatic TSG file generation
        generate_tsg: true,

        // Don't overwrite existing TSG files
        force_overwrite: false,
    };

    // Run the indexer
    match run_indexer(config) {
        Ok(_) => println!("Indexing completed successfully!"),
        Err(e) => {
            eprintln!("Error during indexing: {}", e);
            for (i, cause) in e.chain().enumerate() {
                if i == 0 {
                    continue;
                }
                eprintln!("  caused by: {}", cause);
            }
        }
    }
}
