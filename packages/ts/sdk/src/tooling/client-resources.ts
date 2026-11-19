/** Name one resource per nominal definition, preserving transparent WIT aliases. */
interface ResourceGraph {
  readonly types: readonly {
    readonly id: number;
    readonly definition: {
      readonly kind: string | { readonly type?: string | number | null | undefined };
      readonly owner?: string;
      readonly name?: string;
    };
  }[];
}

/** Generated names, nominal identities and transparent alias declarations. */
interface ResourceNames {
  readonly names: ReadonlyMap<number, string>;
  readonly nominals: ReadonlyMap<number, string>;
  readonly aliases: readonly string[];
  canonical(id: number): number;
}

/** Group local and imported resource aliases by their original WIT definition. */
export function resourceNames(
  graph: ResourceGraph,
  resources: Readonly<Record<string, number>>,
  identity: string,
  occupied: Set<string>,
): ResourceNames {
  const definitions = new Map(graph.types.map(({ id, definition }) => [id, definition]));
  const canonical = (id: number): number => {
    const kind = definitions.get(id)?.kind;
    return typeof kind === "object" && typeof kind.type === "number" ? canonical(kind.type) : id;
  };
  const groups = new Map<number, string[]>();
  for (const [name, id] of Object.entries(resources)) {
    const key = canonical(id);
    const aliases = groups.get(key) ?? [];
    aliases.push(name);
    groups.set(key, aliases);
  }
  const names = new Map<number, string>();
  const nominals = new Map<number, string>();
  const aliases: string[] = [];
  const allocate = (name: string): string => {
    let spelling = name
      .split("-")
      .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
      .join("");
    while (occupied.has(spelling)) spelling += "_";
    occupied.add(spelling);
    return spelling;
  };
  for (const [id, spellings] of groups) {
    const definition = definitions.get(id);
    if (definition?.kind !== "resource" || !definition.owner || !definition.name)
      throw new Error(`${identity}: resource has no portable nominal identity`);
    const preferred =
      definition.owner === identity && spellings.includes(definition.name)
        ? definition.name
        : spellings[0]!;
    const name = allocate(preferred);
    names.set(id, name);
    nominals.set(id, `${definition.owner}#${definition.name}`);
    for (const spelling of spellings) {
      if (spelling === preferred) continue;
      const alias = allocate(spelling);
      aliases.push(
        `/** The transparent WIT resource alias \`${spelling}\`. */`,
        `export type ${alias} = ${name};`,
        "",
      );
    }
  }
  return { names, nominals, aliases, canonical };
}
