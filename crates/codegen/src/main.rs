use std::{
    io::{Read, Write},
    path::PathBuf,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = args.next().map(PathBuf::from);
    let output = args.next().map(PathBuf::from);
    let options = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err(
            "usage: tembra-codegen [descriptor.json|-] [output.rs|-] [bindings.json]".into(),
        );
    }
    let source = match input.filter(|path| path.as_os_str() != "-") {
        Some(path) => std::fs::read_to_string(path)?,
        None => {
            let mut source = String::new();
            std::io::stdin().read_to_string(&mut source)?;
            source
        }
    };
    let contract = tembra_contracts::Contract::from_json(&source)?;
    let options = match options {
        Some(path) => {
            let source = std::fs::read_to_string(path)?;
            let value = tembra_contracts::parse_json(&source)?;
            serde_json::from_value(value)?
        }
        None => tembra_codegen::GenerationOptions::default(),
    };
    let generated = tembra_codegen::generate_rust_with_options(&contract, &options)?;
    match output.filter(|path| path.as_os_str() != "-") {
        Some(path) => std::fs::write(path, generated)?,
        None => std::io::stdout().write_all(generated.as_bytes())?,
    }
    Ok(())
}
