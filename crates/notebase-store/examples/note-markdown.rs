//! Print one note's markdown (or plain text) from a vault.
//!
//! ```text
//! cargo run -q -p notebase-store --example note-markdown -- --vault x.db --id <uuid> [--plain]
//! ```

use notebase_store::Vault;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut vault_path = String::new();
    let mut id = String::new();
    let mut plain = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--vault" => vault_path = args.next().ok_or("--vault needs a path")?,
            "--id" => id = args.next().ok_or("--id needs a note id")?,
            "--plain" => plain = true,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let vault = Vault::open(&vault_path)?;
    let note = vault.load(&id)?.ok_or("note not found")?;
    if plain {
        print!("{}", note.plain_text());
    } else {
        print!("{}", note.to_markdown());
    }
    Ok(())
}
