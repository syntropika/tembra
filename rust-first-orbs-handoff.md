# Rust-first Orbs: resoconto e da fare

- **Aggiornato:** 2026-11-11, 18:20 CET
- **PR:** https://github.com/syntropika/tembra/pull/1 (draft)
- **Stato:** in attesa di Javier. Non toccare la PR finché
  il base non è chiaro.

Questo file è l'unico contenuto del branch
`notes/rust-first-orbs` di origin, separato dalla PR. Serve
alla sessione che riprende il lavoro, anche da un'altra
macchina: si parte da «Inizia qui». Per aggiornarlo si fa
commit e push sullo stesso branch, dal worktree
`.worktrees/notes`.

## Inizia qui: sessione sul server

Primo messaggio da incollare nella sessione:

```text
Leggi rust-first-orbs-handoff.md sul branch
notes/rust-first-orbs di origin. Prepara il clone
come dice «Inizia qui», poi riprendi da
«Alla ripresa, sempre».
```

1. Leggere questo file da un clone qualunque:

   ```bash
   git fetch origin
   git show \
     origin/notes/rust-first-orbs:rust-first-orbs-handoff.md
   ```

2. Preparare i worktree, ognuno con il suo `target/`:

   ```bash
   git worktree add .worktrees/notes \
     notes/rust-first-orbs
   git worktree add .worktrees/lifecycle \
     feat/rust-first-orbs-lifecycle
   ```

   La PR originale (`feat/rust-first-orbs`) serve solo per la
   strada A, che ha i suoi passi.

3. Installare gli strumenti come dice `DEVELOP.md`, sezione
   «Tools»:
   - mise installa bun, node e just;
   - rustup legge `rust-toolchain.toml`: Rust 1.99.0 con
     `wasm32-unknown-unknown`, linker `wasm-component-ld`
     compreso;
   - `just install-tools` aggiunge cargo-machete e
     wasm-bindgen.

   ```bash
   mise trust && mise install
   mise exec -- just install-tools
   mise exec -- just install
   ```

   L'esempio scritto a mano (`rust-orb-lifecycle`) usa anche
   la nightly fissata nel justfile:

   ```bash
   rustup toolchain install nightly-2026-10-15 \
     --profile minimal --target wasm32-unknown-unknown
   ```

4. Prova veloce, dal worktree `.worktrees/lifecycle`:

   ```bash
   cargo test -p tembra-orb-macro -p cargo-tembra
   ./examples/rust-orb-state/run.sh
   ```

   I `run.sh` usano `cargo-tembra` dal `PATH` se c'è,
   altrimenti lo compilano dal workspace. L'output atteso è
   nel README di ogni esempio.

5. Per lavorare con Giacomo:
   - rispondere in italiano, un argomento alla volta, con
     esempi eseguibili;
   - blocchi di codice con righe sotto i ~60 caratteri,
     tabelle strette, niente ASCII art larga;
   - nomi che dicono cosa fanno, e tutto esplicito tramite
     attributi;
   - commit locali in stile conventional commits, push solo
     quando lo chiede, e i force-push li lancia lui;
   - la memoria di Claude del suo Mac non è sul server: le
     decisioni che contano sono in questo file.

## In breve

- PR #1 è `feat/rust-first-orbs` a `d1c4e12`: 52 commit sopra
  `b08f386`, con base `feat/wasm-orb-runtime`. La CI è rossa.
- Oggi il base è cambiato tre volte: prima è arrivata la
  migrazione dei sorgenti a componenti (ABI `tembra:orb`), poi
  Javier l'ha revertita. Ora il base è `43bf592`, con lo stesso
  albero di `7cec028`.
- Javier sta facendo altri cambiamenti: aspettiamo prima di
  scegliere.
- Sono pronte due strade, entrambe su origin:
  - **A.** La PR originale si ribasa su `43bf592` senza
    conflitti. Basta correggere un test del base.
  - **B.** Il branch convertito su `tembra:orb`
    (`feat/rust-first-orbs-lifecycle`) serve solo se la
    migrazione torna.
- Il force-push del convertito su PR #1, chiesto oggi, è
  sospeso: sul base attuale non compila.

## Dove sta il lavoro

Su origin, quindi visibili da qualunque clone:

- `feat/rust-first-orbs` a `d1c4e12`: è la PR #1. Sul Mac di
  Giacomo è il checkout principale.
- `feat/rust-first-orbs-lifecycle` a `03a24b6`
  - è il branch convertito: 4 commit sopra `6e13ad7`;
  - serve per la strada B;
  - sul Mac di Giacomo sta nel worktree
    `.worktrees/rust-counter-tembra-orb`.
- `backup/wasm-components-20261111` a `0dba202`: la copia di
  Javier della migrazione revertita.
- `notes/rust-first-orbs`: questo file.

Solo sul Mac di Giacomo, e non servono altrove:

- `spike/rust-counter-tembra-orb` a `3a00876`: è già il
  primo commit del branch convertito.
- `archive/study-wasm-orbs` a `e51c7c4`: vecchia storia di
  studio, da non toccare.

Commit del branch convertito, dal più vecchio:

- `3a00876` docs(examples): contatore scritto a mano su
  `tembra:orb`
- `31caa45` feat(orb-macro, cargo-tembra): Rust-first orbs
  sul lifecycle `tembra:orb`
- `b225c12` docs(examples): i nove esempi con `#[orb]`
- `03a24b6` docs: README e guide

## Il base oggi

`feat/wasm-orb-runtime`, orari dell'11/11:

| Ora   | Commit    | Cosa                              |
| ----- | --------- | --------------------------------- |
| —     | `b08f386` | merge-base della PR               |
| 02:23 | `7cec028` | base della run CI rossa           |
| 07:38 | `6e13ad7` | sorgenti come componenti          |
| 13:45 | `0dba202` | perf: interprete condiviso        |
| 14:44 | `43bf592` | revert di `6e13ad7` e `0dba202`   |

- `6e13ad7` introduceva l'ABI privato `tembra:orb@0.1.0`:
  - export `lifecycle`: describe, create, release, validate,
    open, close;
  - import `host`: stato, risorse, hook, riferimenti, log,
    contesto, select.
- Il messaggio di `43bf592` dice che la migrazione è
  conservata su `backup/wasm-components-20261111`.
- `origin/main` è `d5ada2d` (v0.2.2, 6/11). Il `main` locale è
  indietro di 5 commit. Il base ha 28 commit che `main` non ha.
- Alle 16:40 Javier non aveva pushato altro dopo `43bf592`.

## CI della PR #1

- **`quality` sul push:** verde.
- **`quality` sulla pull_request:** rossa, perché il merge col
  base non compila:
  `error[E0425]: cannot find function encode_orb_contributions`.
  - Il test del base
    `source_artifacts_admit_only_whole_async_byte_stream_results`
    (`crates/contracts/src/artifact_tests.rs:712` nel merge)
    chiama ancora la funzione che la PR ha rinominato in
    `encode_orb(identity, contract, &Manifest)`.
- **`acceptance`:** rossa su test del base sensibili ai tempi.
  Sono rossi anche sul base da solo e la PR non tocca quei
  percorsi:
  - `streams.rs` (`a_trap_is_never_a_clean_end`);
  - `stream_imports.rs` (producer inattivo);
  - `resources.rs`;
  - `source_stream_imports.rs` (tempo di Pulley).
- **Rilascio** (`package`, gira solo sui tag):
  - è già rotto sul base, perché `contracts-wasm` ha una
    dipendenza path senza versione;
  - con la PR si romperebbe anche `tembra-orb-macro`, per
    l'`include_str!` di `guest.wit` fuori dal crate.
- **Merge di `d1c4e12` su `43bf592`:** pulito (verificato con
  `git merge-tree`); resta solo l'E0425. Dopo il fix va rifatto
  `just check`: la CI si fermava alla compilazione, quindi i
  test del merge non sono mai girati.

## Review: 15 problemi

Review `/code-review high` di `d1c4e12`, tutti CONFIRMED.
Righe riferite a `d1c4e12`.

Legenda:

- **A** = aperto
- **R** = risolto
- **—** = non si applica

Colonne:

- **PR** = la PR originale ribasata su `43bf592`
- **Conv.** = il branch convertito

| #  | Problema                          | PR | Conv. |
| -- | --------------------------------- | -- | ----- |
| 1  | Il merge non compila (E0425)      | A  | —     |
| 2  | `tembra:guest` rifiutato          | —  | R     |
| 3  | `context::log` va in trap         | A  | R     |
| 4  | Reload non validato               | A  | R     |
| 5  | Discriminanti e serde persi       | A  | A     |
| 6  | JSON di stato: NaN e u64          | A  | A     |
| 7  | `contract/` non consumabili       | A  | A     |
| 8  | Input di hook stringa/lista       | A  | A     |
| 9  | `Binding::Hook` senza hook        | A  | R     |
| 10 | `cancel()` ritira lo Store        | A  | R     |
| 11 | Nomi Rust → WIT invalido          | A  | A     |
| 12 | Tipi non usati rifiutati          | A  | A     |
| 13 | `#[wit_resource]` WIT-first       | A  | A     |
| 14 | `Option<Risorsa>` come risultato  | A  | A     |
| 15 | Nessuna CI compila un guest       | A  | A     |

### Dettagli

1. **`crates/contracts/src/artifact.rs:162`.**
   - La PR rinomina `encode_orb_contributions` in
     `encode_orb(identity, contract, &Manifest)` e
     `Artifact.contributions` in `manifest`.
   - Fix sul ribase, nel test del base:
     `&BTreeMap::new()` diventa `&Manifest::default()`
     (`Manifest` deriva `Default`).
   - Poi controllare che l'import di `BTreeMap` non resti
     inutilizzato (clippy `-D warnings`).
2. **`crates/contracts/src/standard.rs:24`.**
   - Con `6e13ad7`, `Contract::source_world` rifiuta ogni
     Resolve che contenga un package `tembra`.
   - La PR mette `tembra:guest` nel Resolve standard, quindi
     ogni build TS sorgente fallirebbe con
     `the tembra namespace is reserved`.
   - Sul base attuale il controllo non c'è più. Torna se
     torna la migrazione.
3. **`crates/executor-wasm/src/component/worker.rs:1302`.**
   - `context::log` diventa la richiesta host `console`.
   - Node e mobile la rifiutano (`scope.ts:138`, "Console
     records are consumed by the interpreter port").
   - `settle_context` (riga 1323) trasforma il rifiuto in un
     trap, e lo Store va in quarantena.
   - Su `43bf592` `Request::Console` esiste di nuovo: sull'host
     nativo funziona, su Node e mobile no.
4. **`crates/orb-macro/src/lib.rs:2780`.**
   - Uno `#[state]` senza `#[validate]` dichiara
     `validated: false`.
   - Al reload il kernel adotta il JSON vecchio senza chiedere
     alla nuova istanza se sa leggerlo. Il primo `read()` va in
     trap (riga 2899) e tutta la revisione va in quarantena.
   - Alzare la versione non aiuta: il kernel risponde
     `native state version changes require a migration
     adapter`.
   - Fix: dichiarare sempre `validated: true` e deserializzare
     nel callback di validazione, come fa già il convertito.
5. **`crates/orb-macro/src/lib.rs:1377`.**
   - Quando un tipo entra nel WIT, la macro sostituisce l'item
     dell'autore con `pub use exports::..::T`.
   - Così spariscono senza errori:
     - i discriminanti espliciti (`High = 90` diventa 2);
     - gli attributi `#[serde(..)]`: il JSON dello stato cambia
       forma appena un metodo cita il tipo, e il `read()` va in
       trap.
   - Fix: un errore di compilazione, oppure conversioni `From`
     al posto della sostituzione.
6. **`crates/orb-macro/src/lib.rs:2895`.**
   - Lo stato viaggia come JSON senza controlli: NaN e ±inf
     diventano `null` e non si rileggono più.
   - Su Node e mobile `host-protocol.ts:166` fa `JSON.parse` e
     arrotonda gli interi oltre 2^53. `u64::MAX` non si rilegge.
   - L'host Rust nativo invece è esatto (`arbitrary_precision`).
7. **`crates/orb-macro/src/wit.rs:355` e
   `crates/cargo-tembra/src/component.rs:107` e `:213`.**
   - Il `contract/` scritto dalla CLI omette i package
     standard (`tembra:guest`, `wasi:*`) e quelli da cui
     dipendono i package consumati.
   - Quindi consumarlo con `wit = "../x/contract"` spesso non
     compila (`package 'example:a@1.0.0' not found`).
   - In più `build.rs:94` scrive il `.wit` prima di validarlo.
   - Nel convertito `tembra:guest` non serve più, ma i package
     transitivi mancano ancora.
8. **`crates/orb-macro/src/lib.rs:2490`** (ramo derivato: riga
   2535).
   - Il `run()` di un hook passa l'input per valore.
     wit-bindgen 0.63 vuole `&T`, `&str` o `&[T]` per gli import
     che contengono stringhe o liste, quindi dà E0308.
   - Il convertito ha già `wit::import_param` per gli import
     liberi: va riusato per gli hook.
9. **`crates/contracts/src/selection.rs:121`.**
   - `providers()` lega come `Binding::Hook` ogni import senza
     `requires` che il world anche esporta, senza verificare
     che esista un hook dichiarato.
   - L'installazione riesce, e la prima chiamata va in trap.
     Il base invece rifiutava con `missing Orb provider
     requirement`.
   - Anche node-binding (`lib.rs:142`) perde gli hook
     dichiarati.
10. **`crates/executor-wasm/src/component/worker.rs:1323`.**
    - Dopo `cancel()`, `principal`, `cancel` e `state.update`
      vengono rifiutati, e ogni rifiuto diventa un trap: il
      guest ritira il proprio Store.
    - Fix: restituire un errore al guest, come fa il convertito
      con i `Result`.
11. **`crates/orb-macro/src/wit.rs:16`.** Quattro lacune nella
    traduzione dei nomi:
    - nessun escape `%` per le parole chiave WIT (`from`,
      `list`);
    - nessun controllo di andata e ritorno dei nomi (`GET`
      diventa `Get`);
    - nessun controllo di collisione tra funzioni e tipi
      (`struct Info` con `fn info`);
    - nessun escape delle parole chiave Rust nei bridge
      WIT-first (`move`).
12. **`crates/orb-macro/src/lib.rs:505`.**
    - Ogni struct ed enum del modulo viene tradotto in WIT
      prima del calcolo di raggiungibilità.
    - Così vengono rifiutati anche tipi mai usati: `[u8; 32]`,
      varianti con campi nominati, un `#[resource]` con un
      array. Doc e README promettono il contrario.
13. **`crates/orb-macro/src/lib.rs:2149`.**
    - `reachable()` percorre anche i metodi dei
      `#[wit_resource]` di un'interfaccia implementata, e cerca
      i tipi del package consumato tra le struct del modulo.
    - Risultato: un errore, oppure un panic (riga 1367) se nel
      modulo c'è una struct con lo stesso nome.
    - È la richiesta (a) di Javier: un servizio che implementa
      un'interfaccia WIT esistente con risorse.
14. **`crates/orb-macro/src/lib.rs:1893`.**
    - `derived_result` avvolge nell'handle solo un risultato
      che è direttamente una risorsa.
    - `Option<Session>`, `Result<Session, _>` e `Vec<Session>`
      danno E0053.
    - È la richiesta (b) di Javier: creare una sessione in modo
      opzionale o fallibile.
15. **`crates/orb-macro/src/tests.rs:2`.**
    - I test della macro confrontano solo token, e
      `cargo-tembra` non ha test.
    - Gli esempi `rust-orb-*` sono fuori dal workspace e dalla
      CI.
    - Fix: un job CI che esegue i `run.sh`, oppure
      `cargo check`/trybuild su crate fixture.

## Il branch convertito

`feat/rust-first-orbs-lifecycle`, sopra `6e13ad7`.

Cosa cambia rispetto alla PR:

- **Tolte** le modifiche della PR a contracts, executor e
  node-binding: niente `tembra:guest`, niente `Manifest`, niente
  `Binding::Kernel` o `Binding::Hook`. Il kernel resta quello
  del base.
- **Macro e WIT:**
  - Il WIT pubblico non contiene più `tembra:`.
  - `generate!` usa un world di build inline:
    `<world>-orb { include <world>;`
    `include tembra:orb/source@0.1.0; import <hook>; }`.
  - Al world aggiunge una copia annidata di `orb.wit`
    (`crates/orb-macro/src/tembra-orb.wit`). Il test
    `the_private_package_is_the_contracts_crate_copy` la
    confronta con `tembra_contracts::SOURCE_WIT`.
- **Export `lifecycle`, generato dalla macro:**
  - `describe` elenca servizi (con lifetime e linger), hook
    (alias e composizione) e contributi (priorità). Alias e
    metodi sono in lowerCamel, letti dal proprio WIT
    (`wit::described`).
  - In `create`, la root dichiara hook, riferimenti
    (`reference-declare(id, ["main"])`) e risorse. Ogni oggetto
    servizio dichiara il suo stato, con il validatore sempre
    attivo.
  - Generati anche `validate` e `open`/`close`, che sono async.
- **Runtime:**
  - Il modulo `__tembra_orb` contiene `Root`, `STATES`
    (ambiente → servizio, con il controllo del proprietario),
    `select_hook`, `select_reference` e `read_resource`.
  - Le interfacce importate diventano moduli wrapper, che
    chiamano `select_reference` prima di ogni funzione libera:
    il base rifiuta le chiamate senza `select`.
- **Contesto:**
  - `context::object()` usa l'indirizzo catturato a `create`,
    perché il base risponde `instance-address` solo durante
    le dichiarazioni.
  - `principal`, `cancelled` e `cancel` restituiscono `Result`.
  - `log` va su `host.log`, e il panic handler logga il
    messaggio.
- **Metadati:** nome e api-version vengono letti da
  `[package.metadata.tembra]` nel `Cargo.toml` del guest
  (dipendenza `toml`).
- **Sezione custom:** `tembra:consumed` (la cartella WIT
  consumata) sostituisce `tembra:manifest`.
- **CLI:**
  - Impacchetta con `Contract::source_component_artifact`.
  - Il riepilogo classifica i contributi dal contratto: il
    linker scarta l'import inutilizzato di un contributore.
- **Esempi:** wit-bindgen con la feature `async`, `.expect` sui
  nuovi `Result`, contratti rigenerati.
- **Dipendenze:** `wit-parser` e `wit-component` fissati a
  `=0.262.0`.

Verificato su `6e13ad7`:

- i 10 esempi girano;
- i 9 portati danno un output host identico byte per byte a
  quello della PR;
- passano i 22 test della macro;
- clippy `-D warnings` è pulito su macro, CLI e tutti i guest;
- passano `cargo machete` e oxfmt;
- `cargo tembra new` + build funziona.

Non verificato:

- `just check` completo;
- acceptance;
- Pulley;
- Node e mobile.

Verso il backup `0dba202`:

- il merge testuale è pulito;
- la firma di `source_component_artifact` non cambia;
- non è stato compilato.

## Da fare

### Adesso

- Aspettare Javier.
- Non toccare la PR #1 e non pushare codice nuovo.

### Alla ripresa, sempre

1. Aggiornare e guardare il base:

   ```bash
   git fetch origin --prune
   ```

   ```bash
   git log --oneline -10 origin/feat/wasm-orb-runtime
   ```

2. Capire se `tembra:orb` è tornato (zsh: usare `${B}:`, mai
   `$B:`):

   ```bash
   B=origin/feat/wasm-orb-runtime
   git cat-file -e \
     "${B}:crates/contracts/wit/tembra/orb.wit" \
     && echo "tembra:orb c'è" || echo "assente"
   ```

3. Leggere i commit nuovi di Javier e decidere con Giacomo:
   A, B oppure C.

### Strada A: il base resta senza `tembra:orb`

1. Creare un branch nuovo da `feat/rust-first-orbs` (per
   esempio `feat/rust-first-orbs-rebased`). Usare un worktree
   con il suo `target/`.
2. Lanciare `git rebase origin/feat/wasm-orb-runtime`. Su
   `43bf592` il merge era pulito.
3. Correggere il test del base (problema 1).
4. Lanciare `just check`, poi l'acceptance. I test lenti del
   base falliscono anche senza la PR.
5. Far girare i 9 esempi con la CLI ricompilata dal branch.
6. Correggere i problemi aperti, in ordine di priorità.
7. Il force-push lo lancia Giacomo: prima il backup, poi
   `--force-with-lease` (vedi sotto).

### Strada B: torna la migrazione a componenti

1. Ribasare `feat/rust-first-orbs-lifecycle` sul nuovo base.
   Su `0dba202` il merge era pulito.
2. Se `orb.wit` è cambiato:
   - ricopiarlo in `crates/orb-macro/src/tembra-orb.wit` (il
     test lo segnala);
   - adattare `lifecycle_code` e `runtime_code` in
     `crates/orb-macro/src/lib.rs`.
3. Rifare le verifiche (vedi sotto).
4. Scegliere con Giacomo:
   - force-push su PR #1, che sostituisce i 52 commit;
   - oppure una PR nuova.

   In entrambi i casi va aggiornato il corpo della PR.
5. Correggere i problemi aperti: 5, 6, 7, 8 e da 11 a 15.

### Strada C: Javier cambia ancora direzione

Rivalutare. Il disegno del convertito (dalla macro a
`lifecycle` e `host`) è descritto sopra, e il codice è nel
branch.

### Priorità dei fix, dopo il ribase

1. **4** — al reload si perde lo stato, e la revisione va in
   quarantena.
2. **15** — un job CI che compila i guest. Costa poco e
   protegge tutti gli altri fix.
3. **9 e 10** — trap che ritirano lo Store (solo strada A).
4. **3** — il log su Node e mobile (solo strada A).
5. **8, 14, 13** — errori di compilazione su forme documentate.
   14 e 13 sono le richieste di Javier.
6. **11 e 12** — nomi, e tipi non usati.
7. **5 e 6** — perdite silenziose di forma o di precisione.
8. **7** — contratti consumabili.

## Comandi e trappole

- Mai lo stesso `--target-dir` per due worktree di questo
  repo: cargo servirebbe le rlib dell'altro worktree.
- Claude non può lanciare `git push --force*`: il comando va
  dato a Giacomo.
- Force-push, prima il backup della PR attuale:

  ```bash
  git push origin \
    d1c4e12:refs/heads/archive/rust-first-orbs-d1c4e12
  ```

  poi la sostituzione (`NUOVO` è il branch ribasato):

  ```bash
  git push origin \
    --force-with-lease=feat/rust-first-orbs:d1c4e12 \
    NUOVO:feat/rust-first-orbs
  ```

- Verifica del convertito, dal suo worktree:

  ```bash
  cargo build -p cargo-tembra
  export PATH="$PWD/target/debug:$PATH"
  ./examples/rust-orb-state/run.sh
  ```

  Gli esempi sono `service`, `state`, `context`,
  `hook-contribution`, `hook-declaration`, `service-lifetime`,
  `shared-interface`, `import`, `wit-resource` e `lifecycle`.
  I README citano l'output atteso.
- Test e lint:

  ```bash
  cargo test -p tembra-orb-macro -p cargo-tembra
  ```

  ```bash
  cargo clippy -p tembra-orb-macro -p cargo-tembra \
    --all-targets -- -D warnings
  ```

- Clippy dei guest:
  - con `--release --target wasm32-unknown-unknown`;
  - con `--manifest-path` del crate guest;
  - con un `--target-dir` separato, per esempio nello
    scratchpad.
- Esempi della PR originale:
  1. ricompilare la CLI dal branch: `git archive HEAD` in una
     cartella di prova, build di `cargo-tembra`, e la si mette
     prima nel `PATH`;
  2. lanciare ogni `run.sh`.

  Dopo modifiche a `guest.wit` la CLI va reinstallata.
- Il convertito richiede wit-bindgen con la feature `async`
  nei guest, perché `open` e `close` sono async.

## Decisioni già prese con Giacomo

- Prima il ribase, poi i fix.
- Tutto esplicito tramite attributi.
- I nomi dicono cosa fanno.
