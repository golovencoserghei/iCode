//! `icode` — thin CLI dispatcher. Subcommands (index/serve/doctor/setup/reembed/
//! backup/hook/...) are wired in from M1 onward; each delegates to the engine or
//! serve layer. No business logic lives here.

fn main() -> anyhow::Result<()> {
    // Placeholder until the clap-based dispatcher lands in M1.
    println!("icode v{} (scaffold)", env!("CARGO_PKG_VERSION"));
    Ok(())
}
