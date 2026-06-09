use std::sync::Arc;

use code_index_core::cli;
use code_index_core::extension::{ProcessorRegistry, StandardLanguageProcessor};

fn build_registry() -> ProcessorRegistry {
    let mut reg = ProcessorRegistry::new();
    reg.register(Arc::new(StandardLanguageProcessor::python()));
    reg.register(Arc::new(StandardLanguageProcessor::rust()));
    reg.register(Arc::new(StandardLanguageProcessor::go()));
    reg.register(Arc::new(StandardLanguageProcessor::java()));
    reg.register(Arc::new(StandardLanguageProcessor::javascript()));
    reg.register(Arc::new(StandardLanguageProcessor::typescript()));
    reg.register(Arc::new(StandardLanguageProcessor::php()));
    reg
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run(build_registry()).await
}
