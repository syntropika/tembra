import { defineRegistry } from "../src/index.js";
import type {
  FacetScope,
  ObjectDeclaration,
  Registry,
  RegistryEntry,
  RegistrySnapshot,
  RegistryView,
  TembraHost,
} from "../src/index.js";

// oxlint-disable-next-line typescript/consistent-type-definitions -- Named interfaces exercise JSON metadata without an index signature.
interface Metadata {
  title: string;
  description?: string;
}
// oxlint-disable-next-line typescript/consistent-type-definitions -- Nested interfaces must preserve portable field checking too.
interface NestedMetadata {
  detail: Metadata;
  tags: readonly string[];
}
const nested = defineRegistry<NestedMetadata>("app.nested");
const commands = defineRegistry<Metadata>("app.commands", { conflict: "error" });

function checkTypes(host: TembraHost, scope: FacetScope, self: ObjectDeclaration) {
  self.register(commands, "save", { title: "Save" });
  self.register(nested, "entry", { detail: { title: "Nested" }, tags: ["portable"] });
  self.registerMethod(commands, "run", { title: "Run", description: "Run a command" }, "run");
  // @ts-expect-error: contribution metadata retains the registry's required fields.
  self.register(commands, "invalid", {});
  // @ts-expect-error: method contributions use the same metadata contract.
  self.registerMethod(commands, "invalid", { title: 2 }, "run");
  // @ts-expect-error: typed contributions cannot hide executable metadata.
  self.register(commands, "invalid", { title: "Wrong", execute: () => 0 });
  const view: RegistryView<Metadata> = host.registry(commands);
  const scoped: RegistryView<Metadata> = scope.registry(commands);
  const table: RegistrySnapshot<Metadata> = view.table();
  const entry: RegistryEntry<Metadata> | undefined = table.get("save");
  const title: string | undefined = entry?.data.title;
  const description: string | undefined = entry?.data.description;
  scoped.subscribe((snapshot) => {
    const subscribed: string | undefined = snapshot.entries[0]?.[1].data.title;
    void subscribed;
  });
  // @ts-expect-error: consumer metadata is not erased to untyped JSON.
  const wrong: number | undefined = entry?.data.title;
  // @ts-expect-error: registry metadata is invariant across contribution and consumer APIs.
  const erased: Registry = commands;
  void [title, description, wrong, erased];
}

// @ts-expect-error: executable metadata is not portable JSON.
defineRegistry<{ execute: () => void }>("invalid");
// @ts-expect-error: native Date values need an explicit managed operation, not metadata.
defineRegistry<{ created: Date }>("invalid-date");
// @ts-expect-error: required undefined values cannot be serialized as JSON.
defineRegistry<{ missing: undefined }>("invalid-undefined");
// @ts-expect-error: optional executable properties are not portable metadata either.
defineRegistry<{ run?: () => void }>("invalid-optional-callback");
// @ts-expect-error: root undefined is not JSON metadata.
defineRegistry<undefined>("invalid-root");
// @ts-expect-error: callable interfaces do not become portable records.
defineRegistry<() => void>("invalid-callable");
void checkTypes;
