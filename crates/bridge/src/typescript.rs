//! Generate the TypeScript call map from the Rust command table and wire records.

use crate::commands::shapes;
use crate::protocol::{ActionType, EffectType, KernelOp, NativeOp, StepType};
use crate::wire::{DECLARATIONS, Direction, OwnerKind, field_line};

const HEADER: &str = "\
// Generated from crates/bridge/src/{commands,protocol,wire}. Do not edit.
// Regenerate with: TEMBRA_BLESS=1 cargo test -p tembra-bridge --test typescript
import type { Code, Lifetime, Status } from \"./identifiers.ts\";

type ValueOf<T> = T[keyof T];
";

/// Return the complete `packages/ts/sdk/src/bridge/commands.ts` module.
pub fn typescript() -> String {
    let mut out = String::from(HEADER);
    for (name, names) in [
        ("KernelOp", KernelOp::ALL),
        ("NativeOp", NativeOp::ALL),
        ("OwnerKind", OwnerKind::ALL),
        ("ActionType", ActionType::ALL),
        ("EffectType", EffectType::ALL),
        ("StepType", StepType::ALL),
    ] {
        out += &format!("\nexport const {name} = Object.freeze({{\n");
        for (variant, wire) in names {
            out += &format!("  {variant}: \"{wire}\",\n");
        }
        out += &format!("}} as const);\nexport type {name} = ValueOf<typeof {name}>;\n");
    }
    for declaration in DECLARATIONS {
        out += "\n";
        out += &declaration();
    }
    out += "\n/** Commands without input fields. */\nexport type EmptyInput = Record<string, never>;\n";
    let commands = shapes();
    for shape in commands.iter().filter(|shape| !shape.fields.is_empty()) {
        out += "\n";
        out += &docs(shape.docs);
        out += &format!("export interface {}Input {{\n", shape.variant);
        for field in &shape.fields {
            out += &field_line(field, Direction::Input);
        }
        out += "}\n";
    }
    out += "\n/** Every kernel operation with its typed input and the result it determines. */\n";
    out += "export interface KernelCommands {\n";
    for shape in &commands {
        let input = if shape.fields.is_empty() {
            "EmptyInput".to_string()
        } else {
            format!("{}Input", shape.variant)
        };
        let key = property(shape.wire);
        let line = format!("  {key}: {{ input: {input}; output: {} }};\n", shape.output);
        if line.len() <= 101 {
            out += &line;
        } else {
            out += &format!(
                "  {key}: {{\n    input: {input};\n    output: {};\n  }};\n",
                shape.output
            );
        }
    }
    out += "}\n";
    out += "\nexport type KernelInput<K extends KernelOp> = KernelCommands[K][\"input\"];\n";
    out += "export type KernelOutput<K extends KernelOp> = KernelCommands[K][\"output\"];\n";
    out += "\n/** The structured result of one platform exchange. */\n";
    out += "export type KernelOutcome<T> =\n  | { ok: T; error?: undefined }\n  | { ok?: undefined; error: KernelError };\n";
    out
}

/// Quote a property name only when it is not an identifier.
fn property(name: &str) -> String {
    if name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        name.to_string()
    } else {
        format!("\"{name}\"")
    }
}

/// Print Rust documentation as a TypeScript block comment.
fn docs(lines: &[&str]) -> String {
    let text = lines
        .iter()
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        String::new()
    } else {
        format!("/** {text} */\n")
    }
}
