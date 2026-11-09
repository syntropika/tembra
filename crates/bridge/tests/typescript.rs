//! The TypeScript call map is generated from the Rust command table and must stay current.

#[test]
fn generated_typescript_matches_the_command_table() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packages/ts/sdk/src/bridge/commands.ts"
    );
    let generated = tembra_bridge::typescript();
    // TypeScript would silently merge duplicate interfaces; every declared name must be unique.
    let mut names = std::collections::BTreeSet::new();
    for line in generated.lines() {
        let declared = ["export interface ", "export type ", "export const "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix));
        if let Some(rest) = declared {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let kind =
                if line.starts_with("export type ") && names.contains(&format!("const {name}")) {
                    "value-type"
                } else if line.starts_with("export const ") {
                    "const"
                } else {
                    "type"
                };
            assert!(
                names.insert(format!("{kind} {name}")),
                "duplicate declaration {name}"
            );
        }
    }
    if std::env::var_os("TEMBRA_BLESS").is_some() {
        std::fs::write(path, &generated).unwrap();
    }
    let current = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        current == generated,
        "{path} is stale; regenerate with TEMBRA_BLESS=1 cargo test -p tembra-bridge --test typescript"
    );
}
