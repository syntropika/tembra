# Rust-first Orbs: resoconto e da fare

- **Aggiornato:** 2026-11-12, 14:15 CET
- **PR:** https://github.com/syntropika/tembra/pull/1 (draft),
  base `main`
- **Stato:** strada A. La PR #1 è a `7119fcc`, sopra `main`
  (`7966893`): la seconda review del 12/11 e i suoi fix,
  più due commit di Javier. Corpo della PR aggiornato. CI su
  `7119fcc` in corso (run `43224859881` push e `43224872989`
  pull_request). Vedi «Seconda review (12/11)».

Questo file è l'unico contenuto del branch
`notes/rust-first-orbs` di origin, separato dalla PR. Serve
alla sessione che riprende il lavoro, anche da un'altra
macchina: si parte da «Inizia qui». Per aggiornarlo si fa
commit e push sullo stesso branch, dal worktree
`.worktrees/notes`.

## Seconda review (12/11)

Da leggere per primo: riassume il 12/11 pomeriggio.

- Review `/code-review high` della PR a `8dfb26e`, dopo i due
  ribase e la PR #2: dei 15 problemi della prima review i 13
  aperti erano tutti presenti, più una trentina di difetti
  nuovi, quasi tutti riprodotti con fixture o test.
- Fix pushati in fast-forward, ognuno con un test che fallisce
  senza il fix:
  - `8dfb26e..be89e31`: 32 commit (runtime, `cargo tembra`,
    17 della macro, esempi, CI `just test-orb-examples`);
  - Javier ha poi pushato sopra `ca5ac69` (authoring: async,
    std di default, facade `tembra`) e `5ffd831` (root
    completata ≠ cancellata);
  - `5ffd831..7119fcc`: 13 commit ribasati sui suoi, conflitti
    della macro risolti tenendo entrambe le parti (bridge
    async + handle annidati + nomi; prelude solo con
    `no_std`, anche sugli errori; `guest::SOURCE`).
- Verificato su `7119fcc`: `just check` (689 test Rust in 123
  binari, TS 551), clippy, machete, `just test-orb-examples`,
  `just test-orb-macro`; ogni commit passa fmt e check.
- Il report dei 15 findings più gravi è nella sessione
  Claude; esiti: tutti risolti tranne il #6, in parte.

Decisioni prese da Claude su delega di Giacomo («decidi
te», 12/11):

- **#10, cancellazione:** dove esiste una risposta vera, il
  guest la riceve invece di un trap. Il principal è letto
  all'ammissione della chiamata; un secondo `cancel()` non fa
  niente; `update` restituisce il valore che il kernel ha
  tenuto. `hook.run` e le chiamate alle dipendenze restano
  trap, come su `main`. Stesso comportamento nel browser.
- **#6, stato:** resta JSON (lo stato tipato è una domanda
  aperta per Javier nella PR). La macro rifiuta NaN e ±inf
  prima di salvare; la perdita di precisione oltre 2^53 sugli
  host JS è documentata, non risolta.
- **#9, `node-binding`:** il manifest della macro si legge in
  `tembra-contracts` (`EmbeddedManifest`), così anche
  `build()` dell'SDK TS tiene hook e service policy.
- **#7:** `cargo tembra build` scrive in `contract/deps/` i
  package standard da cui il contratto dipende; un altro Orb
  lo consuma con `wit = "../x/contract"`.
- **Sorgenti con `tembra:guest`:** rifiutati
  all'installazione (nativo e browser), non nell'artifact: i
  test TS usano artifact sorgente per leggere il contratto.
- Confermate le scelte dei fix della macro: un tipo condiviso
  va nel primo servizio che lo cita; un `impl Default` scritto
  a mano fa da valore iniziale; `-0.0` diventa `0`; il
  `contract/` vecchio di un world rinominato si cancella;
  `system_capability` è pubblica.

Da sapere:

- Nel worktree `.worktrees/pr` il branch locale
  `feat/rust-first-orbs` ha due commit mai pushati
  (`87fb7d4` lockfile, `727b577` `run.sh` eseguibile): sono gli
  stessi fix già nella PR (`03b7092`, `9cd40da`). Prima di
  lavorarci: `git fetch` e riallinearlo a
  `origin/feat/rust-first-orbs`, senza ripushare quei due.
- `acceptance` può fallire su test di stream intermittenti
  del base (per esempio
  `an_idle_producer_ends_both_calls_at_their_terminal_timeout`):
  rilanciare il job con `gh run rerun <id> --failed`.

Resta da fare:

- #6: interi oltre 2^53 sugli host JS (decisione con Javier:
  JSON con interi a 64 bit come stringhe, o stato tipato).
- `cargo tembra wit --emit` scrive un solo `.wit`, senza
  `deps/`.
- I README degli esempi elencano solo `contract/<world>.wit`.
- Le chiavi: `#[orb]` non genera ancora reference view.

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
   git worktree add .worktrees/pr \
     feat/rust-first-orbs
   git worktree add .worktrees/lifecycle \
     feat/rust-first-orbs-lifecycle
   ```

   La PR #1 (`feat/rust-first-orbs`, worktree `pr`) è la linea
   principale. Il lifecycle serve solo per la strada B.

   Sul server i tre worktree esistono già, in
   `/home/claude/github/tembra/.worktrees/`, e `/.worktrees/`
   è in `.git/info/exclude`. Controllare con
   `git worktree list`.

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

   Sul server (11/11) era già tutto installato, alle versioni
   fissate. `~/github/mise.toml` fornisce anche cargo-machete
   e wasm-bindgen, quindi `just install-tools` non serve. La
   nightly non c'è: serve solo alla strada B.

4. Prova veloce, dal worktree `.worktrees/pr`:

   ```bash
   cargo test -p tembra-orb-macro -p cargo-tembra
   ./examples/rust-orb-state/run.sh
   ```

   I `run.sh` usano `cargo-tembra` dal `PATH` se c'è,
   altrimenti lo compilano dal workspace. L'output atteso è
   nel README di ogni esempio.

   Sul server, a `d1c4e12`: passano i 20 test della macro, e
   l'output di `rust-orb-state` coincide con il README.

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

- Dal 12/11: PR #1 è `feat/rust-first-orbs` a `8dfb26e`, base
  `main` (`7966893`), CI verde. Contiene la PR #2
  (`feat/rust-first-orbs-browser` a `c18a95a`, l'adapter
  browser). Il resto di questa sezione descrive l'11/11, prima
  del ribase.
- PR #1 era `feat/rust-first-orbs` a `d1c4e12`: 52 commit
  sopra `b08f386`, con base `feat/wasm-orb-runtime`. La CI era
  rossa.
- `tembra:orb` non è tornato: siamo sulla **strada A**, la PR
  originale. Il convertito (`feat/rust-first-orbs-lifecycle`)
  resta solo per la strada B.
- Il base è a `865e4c6`. Dopo il revert Javier ha aggiunto:
  - `92010d0`: componenti compilati nel browser, chiamate per
    id di funzione;
  - `f3a0e8f`: reference view con chiave per i compilati, e
    `ComponentRoles` nei metadati dell'artifact;
  - `865e4c6`: tempi di un test.
- Fra le 19:30 e le 19:37 CET Javier ha mergiato il base in
  `main`, in fast-forward: `origin/main` = `865e4c6`. La PR
  #1 punta ancora a `feat/wasm-orb-runtime` e va spostata su
  `main`.
- Il merge della PR non è più pulito: su `92010d0` 3 blocchi,
  sulla punta 31 blocchi in 8 file, più due conflitti che il
  merge testuale non mostra.
- Il ribase è fatto (vedi «Avanzamento»): stesso albero del
  merge di prova, che passa `just check`. Manca il push, che
  lancia Giacomo.

## Dove sta il lavoro

Su origin, dall'11/11 sera:

- `feat/rust-first-orbs-rebased` a `f8d9cf2`: la PR ribasata,
  52 commit sopra `main`. Sul server è nel worktree
  `.worktrees/pr`. Va da cancellare dopo il force-push.
- `archive/rust-first-orbs-d1c4e12` a `d1c4e12`: il backup
  della PR prima del ribase.

Solo sul server, nel clone `/home/claude/github/tembra`:

- `refs/probe/rust-first-orbs-merge` a `a47eeb2`: il merge di
  prova, stesso albero (`777aa65`). Solo riferimento, mai da
  pushare.
- `refs/probe/ci-stream-fixes` a `1ad2bc1`, nel worktree
  `.worktrees/probe` (staccato): `9e4f696` più i sette commit
  di Javier sugli stream, vedi «CI della PR #1».

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

Sul server i worktree `notes`, `pr` (la PR) e `lifecycle`
stanno in `/home/claude/github/tembra/.worktrees/`.

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

`feat/wasm-orb-runtime`, orari UTC dei commit dell'11/11
(CET = UTC + 1):

| Ora   | Commit    | Cosa                              |
| ----- | --------- | --------------------------------- |
| —     | `b08f386` | merge-base della PR               |
| 02:23 | `7cec028` | base della run CI rossa           |
| 07:38 | `6e13ad7` | sorgenti come componenti          |
| 13:45 | `0dba202` | perf: interprete condiviso        |
| 14:44 | `43bf592` | revert di `6e13ad7` e `0dba202`   |
| 16:05 | `92010d0` | browser, chiamate per id          |
| 17:30 | `f3a0e8f` | reference view con chiave         |
| 17:51 | `865e4c6` | fix dei tempi di un test          |

- `6e13ad7` introduceva l'ABI privato `tembra:orb@0.1.0`:
  - export `lifecycle`: describe, create, release, validate,
    open, close;
  - import `host`: stato, risorse, hook, riferimenti, log,
    contesto, select.
- Il messaggio di `43bf592` dice che la migrazione è
  conservata su `backup/wasm-components-20261111`.
- Fra le 19:30 e le 19:37 CET (ora del push, vista dai
  fetch) Javier ha portato `main` da `d5ada2d` (v0.2.2,
  6/11) a `865e4c6`, in fast-forward: niente merge commit,
  niente squash. Base e `main` sono lo stesso commit, e
  `b08f386` è antenato di `main`.
- Non c'è una PR GitHub per il base: Javier ha pushato
  `main` direttamente. Il branch `feat/wasm-orb-runtime`
  esiste ancora.
- Orari esatti dei push (API `repos/.../activity`, CET):
  - 19:32: `main` `d5ada2d..865e4c6`, il fast-forward;
  - 19:46: `feat/wasm-orb-runtime` `865e4c6..aa88fef`;
  - 20:41: `feat/wasm-orb-runtime` `aa88fef..27c608c`, con
    `ff78e94` (tooling: typed WIT imports), `d8b6d53`
    (wasi: compiled operations e cancellation) e `27c608c`
    (test).
- Quindi Javier lavora su `feat/wasm-orb-runtime` e porta
  `main` avanti in fast-forward quando vuole. Alle 21:15 il
  suo branch era 4 commit avanti a `main`.
- I 4 commit toccano due file della PR: il README di
  executor-wasm e `component/worker.rs`. Il merge della PR
  ribasata su `27c608c` è pulito e passa `cargo check`
  sull'intero workspace.

## Il base nuovo: cosa cambia per noi

### `92010d0`: browser e chiamate per id

Per gli Orb compilati:

- Ora girano anche nel browser, ma solo con un adapter
  preparato dal build dell'app: `build({ .., browser: true })`
  dell'SDK TS genera i binding con JCO e strumenta i moduli.
  L'app lo importa e lo registra; il runtime lo accetta solo
  se lo SHA-256 dell'artifact coincide. Serve JSPI.
- Il profilo browser esclude risorse, stream e future tra
  Orb, e le definizioni di hook compilate.
- L'host chiama le funzioni esportate per id (lo slot che
  `bind` restituisce), non più per `(servizio, metodo)`. Il
  worker controlla che l'ambiente sia del servizio della
  funzione; una contribution gira solo nell'ambiente root.
- Nuovo `PreparedComponent` in contracts: legge i metadati di
  un compilato senza verificatore WIT, fidandosi del digest.

Per le nostre modifiche al kernel:

- Conflitti in `worker.rs` (`Environments`,
  `Packet::Create`) e `component.rs` (ruolo dell'import).
- Due rotture che il merge testuale non mostra:
  - `component.rs`: il `match` dei ruoli chiama
    `contributions.contains_key(..)`, ma ora `contributions`
    è un `BTreeSet<u32>` di slot;
  - `worker.rs`: le callback del kernel (`state-init`,
    `resource-init`) stanno nella mappa `exports` con chiave
    `(interfaccia, funzione)`; ora la chiave è lo slot `u32`.
- `PreparedComponent` ignorerebbe in silenzio gli hook
  dichiarati e le policy dei servizi.
- `cargo-tembra` non produce l'adapter browser, e il browser
  non serve gli import `tembra:guest`: `#[state]`,
  `#[resource]`, contesto e hook dichiarati non girano lì.

### `f3a0e8f`: reference view con chiave

Per gli Orb compilati:

- Un import «reference view» sceglie oggetti del kernel per
  chiave: una risorsa che rispecchia i metodi del provider,
  più `get: async func(key: list<string>)`
  `-> result<R, string>`.
- Il ruolo sta nei metadati dell'artifact:
  `ComponentRoles { contributions, references }` prende il
  posto della mappa delle contribution.
  `Contract::component_artifact` prende `&ComponentRoles`,
  l'encoder è `encode_orb_roles`, e il payload cambia formato
  di proposito.
- Un'acquisizione rifiutata torna come errore WIT, senza trap
  e senza quarantena.
- Esempio Rust: `crates/executor-wasm/rust-keyed-router`
  (wit-bindgen, nightly), compilato in CI.

Per le nostre modifiche al kernel:

- `ComponentRoles` è lo stesso concetto del nostro
  `Manifest`, esteso in un altro modo: 12 blocchi in
  `artifact.rs`, 5 in `authoring.rs`, 4 nei test, più
  `compiled_hooks.rs` e node-binding.
- `lower.rs`: accanto ai nostri `Binding::Kernel` e
  `Binding::Hook` arriva il rifiuto «una view vuole un
  provider Orb».
- `worker.rs`: Javier aggiunge `Import.reference: bool` dove
  noi abbiamo `role: Role`, ed estrae `import_authority`,
  lo stesso helper della nostra `issuing_authority`.
- Il test del problema 1 ora chiama `encode_orb_roles`: con i
  nomi di Javier compila così com'è.
- Per la macro è un'occasione: `#[orb]` potrebbe generare la
  view e il ruolo per un import con chiave. Nel guest serve
  wit-bindgen con `async`.

### `865e4c6`

Tocca solo `crates/executor-wasm/tests/resources.rs`. Niente
per noi.

### I problemi aperti, sul base nuovo

- **3:** `scope.ts:162` rifiuta ancora `console` su Node e
  mobile.
- **9:** `selection.rs`, `hooks.rs` e `standard.rs` non sono
  cambiati nel base: il problema resta com'era.
- **10:** per le view Javier restituisce errori WIT invece di
  trap: è la stessa strada del nostro fix.

## CI della PR #1

### Dopo la PR #2 (12/11)

La PR #2 (`feat/rust-first-orbs-browser`, l'adapter browser
per i compilati) è stata mergiata nella PR #1 il 12/11 alle
00:22 CET: la PR #1 è a `9e4f696`, 55 commit sopra `main`.

Run su `9e4f696`: `43119361463` (push) e `43119619184`
(pull_request). `quality` verde. `acceptance` rossa, ogni
volta su un test diverso di `source_stream_imports.rs`:

- `a_source_consumer_reads_native_and_source_producers_`
  `exactly`: «interpreter callback exceeded its wall time
  limit; Store quarantined»;
- `a_source_consumer_reads_a_compiled_producer_exactly_`
  `and_sees_its_trap`: la trap del producer arriva come
  errore dell'import, non come errore di lettura.

Non è la PR. Durata di `source_stream_imports.rs`:

| Ramo   | Commit    | Durata | Esito |
| ------ | --------- | ------ | ----- |
| main   | `865e4c6` | 332 s  | ok    |
| PR     | `f8d9cf2` | 340 s  | ok    |
| PR     | `9e4f696` | 477 s  | rosso |
| PR     | `9e4f696` | 700 s  | rosso |
| Javier | `aa88fef` | 721 s  | rosso |
| Javier | `27c608c` | 686 s  | rosso |
| Javier | `dd52ff0` | 94 s   | ok    |

- Rosse sono le run lente, e lì è lento tutto, anche i test
  che la PR non tocca: dipende dal runner.
- `main` ha una sola run, su `865e4c6`. Dopo, l'acceptance è
  rossa su tutti i commit di Javier, anche senza la PR.
- Javier sta sistemando proprio questi test, su
  `feat/wasm-orb-runtime`. Sette commit, dalle 20:41 CET
  dell'11/11 all'01:40 del 12/11:
  - `27c608c`: meno carico nel test di backpressure;
  - `c89d1c8`: la trap del producer compilato, e in
    `standard.ts` l'iterazione per byte senza una promise
    per byte;
  - `dcbcece`: cache in CI, Cranelift a `opt-level = 3` nei
    test (da qui i 94 s);
  - `0edd61b`, `a4b9592`, `dd52ff0`, `7966893`: tempi e
    canali di errore degli stream; `a4b9592` corregge anche
    `bindings/operations.rs`.
- I sette commit si applicano puliti su `9e4f696`. Risultato
  in `refs/probe/ci-stream-fixes` (`1ad2bc1`), nel worktree
  `.worktrees/probe`: passano `cargo fmt --check` e
  `cargo check --workspace --all-targets`.
- Il merge di tutto `feat/wasm-orb-runtime` (`7966893`) sulla
  PR invece ha conflitti in 7 file: `Cargo.lock`,
  `README.md`, `crates/contracts/src/standard.rs`,
  `docs/rust.md` e tre file dell'adapter browser toccati
  dalla PR #2, in `packages/ts/sdk/src/interpreter/
  component/`: `execution.ts`, `revision.ts`, `scope.ts`.
- CI di Javier su `7966893` (run `43129112003`, finita alle
  02:00 CET): verde, `quality` e `acceptance`.
  `source_stream_imports.rs` in 93 s; tutti i test
  dell'acceptance in 404 s, contro i 1208 s di `main`.
- `just check` su `1ad2bc1`: verde, 609 test Rust in 115
  binari, più TypeScript ed esempi.

La proposta di portare i sette commit nella PR è superata:
alle 02:01 CET Javier ha portato `main` a `7966893`, in
fast-forward, e la PR è stata ribasata lì (sotto).

### Secondo ribase, su `7966893` (12/11)

`main` è passato da `865e4c6` a `7966893`: 15 commit di
Javier, fra cui le correzioni degli stream, la facciata unica
`tembra` (`c77fe93`) e le risorse WIT possedute come argomenti
(`5345542`).

- Merge di prova: `refs/probe/rust-first-orbs-merge-7966893`
  (`9c35ada`), nel worktree `.worktrees/probe`. `just check`
  verde: 613 test Rust in 118 binari, più TypeScript ed
  esempi.
- Ribase: branch locale `rebase/rust-first-orbs-onto-7966893`
  a `8dfb26e` (anche `refs/probe/rust-first-orbs-rebased-
  7966893`), nel worktree `.worktrees/pr`. 55 commit, ognuno
  passa `cargo fmt --check` e `cargo check --workspace
  --all-targets`; il commit browser passa anche
  `bun run typecheck`. Albero `7354261`, identico al merge di
  prova.
- Backup su origin: `archive/rust-first-orbs-9e4f696`.

Conflitti testuali (7 file), risolti tenendo le scelte di
Javier:

- `Cargo.lock`: entrambi i pacchetti nuovi
  (`tembra-orb-macro`, `tembra-runtime`).
- `README.md`: la tabella della PR, con la descrizione nuova
  di `crates/tembra/` («Single Rust facade...»).
- `docs/rust.md`: la frase nuova di Javier sull'esempio
  misto, poi il testo della PR.
- `contracts/src/standard.rs`: Javier ha spostato l'elenco
  WASI in `WASI_SOURCES`, che usa anche il `build.rs` di
  executor-wasm per i binding dei sorgenti. `guest.wit` resta
  fuori da `WASI_SOURCES` e si aggiunge in coda:
  `WASI_SOURCES.iter().chain([&guest])`.
- `interpreter/component/{execution,revision,scope}.ts`
  (PR #2 contro le risorse e gli stream di Javier): import
  uniti; nel costruttore il kernel e il listener `retired`;
  al rilascio prima la pulizia delle risorse di Javier, poi
  `kernel.release`.

Conflitti che il merge testuale non mostra:

- `component/worker.rs`: `Call` ha il campo nuovo
  `arguments` (risorse possedute). Le 4 chiamate del kernel
  della PR (commit dello stato, delle risorse root e della
  validazione) passano `arguments: None`, come la
  `Contribute` di Javier.
- `scope.ts`: `CallbackScope` chiede `report` e
  `releaseResource`. Lo scope dei callback del kernel (PR
  #2) li ha: `report` come lo scope delle invocazioni,
  `releaseResource` chiude subito, perché lì niente può
  riacquisire la risorsa.

Giacomo ha mandato in chat il comando del force-push, e alle
02:27 CET Claude l'ha lanciato:

```text
+ 9e4f696...8dfb26e 8dfb26e -> feat/rust-first-orbs
  (forced update)
```

- CI su `8dfb26e`: run `43134607159` (pull_request) e
  `43134599472` (push), entrambe verdi alle 02:58 CET,
  `quality` e `acceptance`. La PR è `MERGEABLE`/`CLEAN`.
- I nove esempi (`run.sh`) stampano lo stesso output del
  primo ribase. Lanciarli riscrive però i `Cargo.lock` dei
  loro host, perché `main` ha spostato il runtime in
  `tembra-runtime`.
- Sul server, nel worktree `.worktrees/pr` (ora sul branch
  `feat/rust-first-orbs`), due commit locali da pushare con
  l'ok di Giacomo, con push normale:
  - `87fb7d4` chore(examples): i nove `Cargo.lock` degli host
    su `tembra-runtime`;
  - `727b577` fix(examples): `rust-orb-import/run.sh`
    eseguibile (fix 10 della lista).
- Bozza del corpo della PR, da applicare con l'ok di Giacomo:
  `/home/claude/github/tembra/.worktrees/pr-body-7966893.md`.
  Cambia l'intro (base `7966893`), aggiunge «What the rebase
  on `7966893` changed here» e aggiorna «Checks».
- Il classificatore ha bloccato l'annullamento delle run CI
  del branch di backup (`43134173504`) e di quella ferma
  dall'outage (`43095862614`): non servono, ma si possono
  lasciare.

### Prima del ribase (`d1c4e12`)

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
- **Merge di `d1c4e12`:**
  - su `43bf592`: pulito, resta solo l'E0425;
  - su `92010d0`: 3 blocchi in `component.rs` e `worker.rs`;
  - su `865e4c6`: 31 blocchi in 8 file, vedi «Piano del
    ribase».

  La CI si fermava alla compilazione, quindi i test del merge
  non sono mai girati.
- **GitHub, 11/11 sera:** la PR è `CONFLICTING`. Con un
  conflitto GitHub non lancia i workflow `pull_request`:
  fino al ribase la CI della PR non gira.

## Review: 15 problemi

Review `/code-review high` di `d1c4e12`, tutti CONFIRMED.
Righe riferite a `d1c4e12`.

Legenda:

- **A** = aperto
- **R** = risolto
- **P** = risolto in parte
- **—** = non si applica

Colonna **PR** aggiornata al 12/11 pomeriggio (`7119fcc`),
dopo la seconda review.

Colonne:

- **PR** = la PR originale, ribasata
- **Conv.** = il branch convertito

| #  | Problema                          | PR | Conv. |
| -- | --------------------------------- | -- | ----- |
| 1  | Il merge non compila (E0425)      | R  | —     |
| 2  | `tembra:guest` rifiutato          | —  | R     |
| 3  | `context::log` va in trap         | R  | R     |
| 4  | Reload non validato               | R  | R     |
| 5  | Discriminanti e serde persi       | R  | A     |
| 6  | JSON di stato: NaN e u64          | P  | A     |
| 7  | `contract/` non consumabili       | R  | A     |
| 8  | Input di hook stringa/lista       | R  | A     |
| 9  | `Binding::Hook` senza hook        | R  | R     |
| 10 | `cancel()` ritira lo Store        | R  | R     |
| 11 | Nomi Rust → WIT invalido          | R  | A     |
| 12 | Tipi non usati rifiutati          | R  | A     |
| 13 | `#[wit_resource]` WIT-first       | R  | A     |
| 14 | `Option<Risorsa>` come risultato  | R  | A     |
| 15 | Nessuna CI compila un guest       | R  | A     |

### Dettagli

1. **`crates/contracts/src/artifact.rs:162`.**
   - La PR rinomina `encode_orb_contributions` in
     `encode_orb(identity, contract, &Manifest)` e
     `Artifact.contributions` in `manifest`.
   - Superato da `f3a0e8f`: il test del base ora chiama
     `encode_orb_roles(.., &ComponentRoles::default())`. Con la
     scelta D1 del piano la PR tiene quei nomi, e il test
     compila senza ritocchi.
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
   - Al `865e4c6` il rifiuto è a `scope.ts:162`.
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

## Piano del ribase (strada A)

Proposto l'11/11 alle 19:35 CET. **Aspetta l'ok di Giacomo.**

Bersaglio: `origin/main`, oggi `865e4c6`, cioè la punta del
base mergiata in fast-forward. Conflitti e scelte sono quelli
calcolati sulla punta: stesso commit.

La PR #1 va spostata da `feat/wasm-orb-runtime` a `main`:

```bash
gh pr edit 1 --base main
```

- Oggi il diff della PR non cambia: i due branch sono lo
  stesso commit.
- Meglio farlo presto: se il branch del base viene
  cancellato, GitHub chiude la PR. Il retarget automatico
  scatta solo cancellando il branch di una PR mergiata, e
  il base non ne aveva una.
- Tocca la PR, quindi serve l'ok di Giacomo.

### Conflitti sulla punta

| File                     | Blocchi | Tema                  |
| ------------------------ | ------- | --------------------- |
| contracts `artifact.rs`  | 12      | Manifest / Roles      |
| contracts `authoring.rs` | 5       | Manifest / Roles      |
| `artifact_tests.rs`      | 4       | Manifest / Roles      |
| contracts `lower.rs`     | 2       | Kernel, Hook e view   |
| executor `component.rs`  | 1       | ruolo dell'import     |
| `component/worker.rs`    | 5       | campi, helper, create |
| `compiled_hooks.rs`      | 1       | Manifest / Roles      |
| node-binding `lib.rs`    | 1       | Manifest / Roles      |

In più i due conflitti semantici di `92010d0` (vedi sopra), e
`port.rs`, che passa `&artifact.manifest`.

Le fermate del ribase sono i 10 commit della PR che toccano
quei file: 11, 12, 16, 17, 23, 24, 30, 32, 33 e 36, cioè
`337576a`, `18d017a`, `2399ae3`, `d754d9f`, `1a0050e`,
`779e238`, `68ca53c`, `53d1b5c`, `4bff698` e `27728f7`.
I pezzi in conflitto nascono qui:

- `18d017a` (12): `Role`, `issuing_authority` e la mappa
  delle callback del kernel;
- `1a0050e` (23): gli hook nel payload `tembra:orb`;
- `68ca53c` (30): `Manifest`.

### Avanzamento

Ok di Giacomo poco dopo le 19:40 CET: «se possibile teniamo
le scelte di Javier; se ci bloccano o non sono una buona
idea, cambia di conseguenza». Quindi le scelte qui sotto sono
state rivedute:

- **D1:** `Artifact` tiene il campo `contributions` di
  Javier (le view restano nel contratto) e aggiunge `hooks`
  e `services`. Niente `Artifact.roles`, che duplicherebbe le
  reference. `describe` riceve le tre mappe esplicite.
- **Payload:** contributions, hooks, services, references.
  Il test di Javier `source_artifacts_and_duplicate_roles_`
  `are_refused` vuole le reference in fondo, e resta anche
  l'ordine della PR.
- **D2:** resta il `reference: bool` di Javier accanto al
  `role: Role` della PR: una view è una dipendenza Orb che si
  acquisisce per chiave.
- **D3:** resta `import_authority(state)` di Javier, con i
  suoi chiamanti; il corpo delega a `issuing_authority`.
- **D5:** il braccio di Javier diventa
  `Host | Kernel | Hook if reference.is_some()`.
- **D4 e D6:** come sotto.
- `manifest.rs` diventa `roles.rs` (resta `ServicePolicy`),
  e il test `compiled_manifest.rs` diventa
  `compiled_service_policies.rs`.

Passi fatti:

- **Spostamento della PR su `main`:** Giacomo ha detto «fai
  te», ma il classificatore della modalità automatica ha
  bloccato `gh pr edit`. Va lanciato da Giacomo, o va dato
  il permesso.
- **Passo 1, merge di prova:** worktree `.worktrees/probe`,
  commit locale `a47eeb2`, ref
  `refs/probe/rust-first-orbs-merge` (mai pushato).
  - `cargo clippy --workspace --all-features --all-targets`
    con `-D warnings`: pulito.
  - `just check`: verde, con 609 test Rust in 115 binari e i
    test TS.
- Due trappole trovate nel merge «pulito»:
  - `executor-wasm/tests/hooks.rs` univa i due involucri
    (`Manifest` dentro `ComponentRoles`): torna quello di
    Javier;
  - `contracts/src/prepared_tests.rs` aveva `encode_orb` a
    tre argomenti: torna quello di Javier.
- **Esempi sul merge di prova:** i 9 `rust-orb-*` danno lo
  stesso output della PR originale. Cambia solo la dimensione
  dell'artifact, +1 byte: la mappa vuota delle reference.
- **Passo 2, ribase:** `feat/rust-first-orbs-rebased` a
  `f8d9cf2`, nel worktree `.worktrees/pr`.
  - Fermate: per conflitto 5, 11, 12, 14, 17, 23, 24, 26,
    30, 31, 36 e 43; perché non compilavano 2, 16, 33 e 39.
  - Nella storia originale 16 e 23 non compilavano già: il
    16 rinominava `guest::exports_state_init` senza aggiornare
    l'executor, e il 23 aggiungeva `Binding::Hook` senza il
    braccio nell'executor. Ora compilano: il 23 rifiuta
    `Binding::Hook` finché il 24 non lo serve.
  - Messaggi riscritti: 23 (`CallbackRoles` diventa
    `ComponentRoles`), 24, 30 («component roles carry service
    lifetimes»), 31 e 39. Nei riscritti c'è anche la riga
    `Co-Authored-By` di Claude Opus 5.5.
  - Autosquash finale con `cargo fmt --all --check` e
    `cargo check --locked --workspace --all-features`
    `--all-targets` dopo **ogni** commit: tutti verdi.
- **Passi 3 e 4, verifica:**
  - l'albero del ribase è identico a quello del merge di
    prova (`777aa65`), quindi valgono `just check`, clippy e
    gli esempi fatti lì;
  - acceptance non eseguita: serve Chrome, che sul server
    non c'è, più la nightly fissata, wasi-sdk 34 e la build
    dell'interprete. La lancia la CI dopo il push.
- **Trovato:** `examples/rust-orb-import/run.sh` ha modo
  `100644`, quindi `./run.sh` dà «Permission denied» (con
  `sh run.sh` gira). È già così nella PR: è un fix da fare.

### Scelte proposte (rivedute in «Avanzamento»)

- **D1. Un solo tipo per i metadati del compilato:**
  `ComponentRoles` di Javier, con in più `hooks` e
  `services` della PR.
  - `Manifest` sparisce; `Artifact.manifest` diventa
    `Artifact.roles`.
  - Il payload scrive contributions, references, hooks e
    services, in quest'ordine.
  - `check_manifest` diventa `check_roles`. Le view restano
    su `reference_bindings` e `with_references`.
  - Restano i nomi di Javier (`encode_orb_roles`, `encode_orb`
    a due argomenti): i suoi test compilano senza ritocchi, e
    il problema 1 sparisce.
  - Quasi tutti i suoi letterali usano già
    `..Default::default()`; nessuno deserializza
    `ComponentRoles` da JSON.
- **D2. Nel worker, `Role` della PR più la variante
  `Reference`**, al posto di `reference: bool`.
  - `declare` salta ciò che non è `Dependency`: le view non
    ricevono `main`.
  - `link_imports` tratta `Reference` come `Dependency`, e il
    ramo `acquire` di Javier serve `get`.
- **D3. Un solo helper per l'autorità:**
  `issuing_authority(state, missing)` della PR. I due
  chiamanti di Javier gli passano il suo messaggio.
- **D4. Le callback del kernel in una mappa a parte**,
  `Exports.kernel`, con chiave `(interfaccia, funzione)`.
  `Exports.functions` resta di Javier (slot → `Target`).
- **D5. `lower.rs`:** tenere i due lati, e allargare il
  rifiuto «una view vuole un provider Orb» a ogni binding
  non-Orb (Host, Kernel, Hook).
- **D6. `PreparedComponent::decode`** (browser) rifiuta un
  artifact con hook dichiarati o policy dei servizi, invece
  di ignorarli.

Il resto è meccanico:

- `Environments`: unione dei campi. In `Packet::Create` va la
  `declare` della PR, poi `services.insert` se riesce. In
  `Release` si toglie l'ambiente da tutte le mappe.
- `component.rs`: il ruolo `Contribution` si legge da
  `hooks.contributions`; `view` va calcolata prima del ruolo.
- `port.rs`, `cargo-tembra` e i test della PR: da `Manifest`
  a `ComponentRoles`; node-binding con `..Default::default()`.

### Passi

0. `git fetch` e controllo che `origin/main` sia ancora
   `865e4c6`. Se si è mosso, rifare l'analisi.
1. **Merge di prova** in un worktree staccato e usa e getta
   (`.worktrees/probe`, mai pushato): applicare D1–D6, poi
   `cargo check`, poi `just check`. Così ogni risoluzione è
   fissata e verificata prima di toccare la storia. L'albero
   risolto si salva in un ref locale.
2. **Ribase** nel worktree `pr`, su un branch nuovo:

   ```bash
   CHECK='cargo check --locked --workspace'
   CHECK="$CHECK --all-features --all-targets"
   git switch -c feat/rust-first-orbs-rebased
   git -c rerere.enabled=true rebase -x "$CHECK" \
     origin/main
   ```

   Le opzioni sono quelle di `just lint` e `just test`.

   - Alle 10 fermate si risolve come nel merge di prova.
   - `-x` fa compilare ogni commit, anche quelli che si
     applicano senza conflitti ma usano ancora `Manifest`
     (31 e 39).
   - Il messaggio di `68ca53c` va riscritto: `Manifest` non
     c'è più.
3. **Stesso albero:** `git diff <prova> HEAD` deve essere
   vuoto, o con differenze spiegate.
4. **Verifiche:**
   - `just check`;
   - acceptance (i test lenti del base falliscono anche
     senza la PR);
   - i 9 esempi con la CLI ricompilata, confrontati coi
     README;
   - clippy `-D warnings` su macro e CLI.
5. Aggiornare questo file. Il force-push lo lancia Giacomo
   (vedi «Comandi e trappole»).

### Aperti dal base nuovo, dopo il ribase

- **Browser:** `cargo-tembra` non produce l'adapter, e gli
  import `tembra:guest` non sono serviti nel browser.
- **Chiavi:** `#[orb]` non sa ancora generare una reference
  view; oggi un import lega solo `main`.

## Da fare

### Adesso

Fatto da Claude verso le 20:30 CET, su richiesta di Giacomo
(«Non puoi farlo te?»):

- PR #1 spostata su `main` (`gh pr edit 1 --base main`; alla
  seconda prova il classificatore l'ha lasciato passare);
- backup `archive/rust-first-orbs-d1c4e12`;
- push del ribase come branch nuovo
  `feat/rust-first-orbs-rebased`. La CI `Quality` gira sul
  push: run `43088224249`, `quality` e `acceptance`.

Il force-push al primo tentativo è stato bloccato dal
classificatore («Git Destructive»). Giacomo, in Remote Control
e senza SSH, ha mandato il comando esatto in chat (il prefisso
`!` non passa dal Remote Control: arriva come messaggio), e
alle 21:24 CET Claude l'ha lanciato:

```text
+ d1c4e12...f8d9cf2 feat/rust-first-orbs-rebased
  -> feat/rust-first-orbs (forced update)
```

CI sulla PR: run `43095862614` (push) e `43095868722`
(pull_request), su `f8d9cf2`. Alle 22:00 CET GitHub Actions
era in «major outage»: la run pull_request è finita come
failure senza mai partire (job in coda, zero step), la push è
rimasta in coda. Alle 22:50 Actions era di nuovo operativo e
Claude ha rilanciato la run pull_request (tentativo 2):

```bash
gh run rerun 43095868722
```

Esito della run `43088224249` sul branch di passaggio, stesso
commit `f8d9cf2`:

- `quality` verde;
- `acceptance` rossa su due test di `source_stream_imports.rs`:
  `the_head_arrives_early_and_the_producer_is_backpressured`
  (limite di tempo dell'interprete) e
  `a_source_consumer_reads_a_compiled_producer_exactly_`
  `and_sees_its_trap`.
- Non sono della PR: le run di Javier sui suoi commit, senza
  la PR, falliscono sugli stessi test (`aa88fef` il primo,
  `27c608c` il secondo, più
  `a_source_consumer_reads_native_and_source_producers_`
  `exactly`). Sul `865e4c6` invece erano passati: sono test
  intermittenti, sensibili ai tempi.

Esito del rilancio della run pull_request (tentativo 2, fino
alle 23:20 CET): `quality` verde; `acceptance` rossa su due
test di `source_streams.rs`, diversi dalla volta prima:
`a_small_packet_limit_never_truncates_a_stream_into_a_clean_`
`end` («standard packet exceeds the packet limit») e
`dropping_a_stream_of_an_idle_producer_ends_its_callback`
(Pulley). La PR non tocca quel percorso (zero modifiche a
`bindings`, `worker.rs` e `adapter.rs` di executor-wasm):
anche questi sono intermittenti. La PR resta `UNSTABLE` solo
per l'acceptance. La run push `43095862614` è ancora in coda
dall'outage: si può annullare, duplica la pull_request.

Poi:

1. cancellare il branch di passaggio (la sua run è finita).
   Per Claude è bloccato dal classificatore («Git
   Destructive»): Giacomo lo cancella dalla pagina dei branch
   di GitHub, oppure manda in chat il comando esatto:

   ```bash
   git push origin --delete \
     feat/rust-first-orbs-rebased
   ```

2. aggiornare il corpo della PR. Fatto da Claude alle 21:27
   CET, con l'ok di Giacomo. La bozza applicata è sul server, in
   `/home/claude/github/tembra/.worktrees/pr-body-865e4c6.md`.
   Applicata con:

   ```bash
   gh pr edit 1 --body-file \
     /home/claude/github/tembra/.worktrees/pr-body-865e4c6.md
   ```

CI sul ribase (run `43088224249`): alle 21:20 `quality` era
verde in 12 minuti, `acceptance` ancora in corso.

Nota: Javier ha pushato `aa88fef` su `feat/wasm-orb-runtime`
(commit delle 19:46 CET, visto verso le 20:30), non ancora in
`main`: «feat(browser): support compiled orb resources and
byte streams». Con la PR ribasata ha in comune solo
`crates/executor-wasm/README.md`, e il merge di prova è
pulito.

Dopo il push: guardare la CI (`quality` e `acceptance`;
alcuni test lenti del base erano già rossi), poi i fix in
ordine di priorità.

### Alla ripresa, sempre

1. Aggiornare e guardare il base, che dall'11/11 sera è
   `main`:

   ```bash
   git fetch origin --prune
   ```

   ```bash
   git log --oneline -10 origin/main
   ```

   Guardare anche `origin/feat/wasm-orb-runtime`, nel caso
   Javier continui lì.

2. Capire se `tembra:orb` è tornato (zsh: usare `${B}:`, mai
   `$B:`):

   ```bash
   B=origin/main
   git cat-file -e \
     "${B}:crates/contracts/wit/tembra/orb.wit" \
     && echo "tembra:orb c'è" || echo "assente"
   ```

3. Leggere i commit nuovi di Javier e decidere con Giacomo:
   A, B oppure C.

### Strada A: il base resta senza `tembra:orb`

È la strada scelta l'11/11.

1. Seguire «Piano del ribase», dopo l'ok di Giacomo.
2. Correggere i problemi aperti, in ordine di priorità.
3. Il force-push lo lancia Giacomo: prima il backup, poi
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

Superata il 12/11 pomeriggio: tutti fatti tranne #6 in parte
(vedi «Seconda review (12/11)»).

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
9. Gli aperti dal base nuovo: browser e chiavi.
10. Il bit eseguibile di `examples/rust-orb-import/run.sh`.

## Comandi e trappole

- Mai lo stesso `--target-dir` per due worktree di questo
  repo: cargo servirebbe le rlib dell'altro worktree.
- Sul server git è 2.34: niente `merge-tree --write-tree`.
  Per un merge di prova serve un worktree staccato:

  ```bash
  git worktree add --detach .worktrees/probe \
    origin/main
  cd .worktrees/probe
  git -c merge.conflictStyle=diff3 merge \
    --no-commit --no-ff origin/feat/rust-first-orbs
  git diff --name-only --diff-filter=U
  git merge --abort
  ```

  `diff3` mostra anche la versione di `b08f386`.
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
- 11/11 sera: `tembra:orb` non è tornato, quindi strada A.
  La PR #1 resta la linea principale; il lifecycle serve solo
  per la strada B.
- 11/11, 21:20: la PR #1 resta su `main`, anche se Javier
  continua su `feat/wasm-orb-runtime`.
- 11/11, 22:50: Giacomo ha mandato a Javier un messaggio con
  le scelte del ribase (le sue tenute dove si poteva, cosa è
  cambiato nel suo codice). Aspettare la sua risposta prima
  di cambiare di nuovo quelle parti.
- 12/11 pomeriggio: Giacomo ha delegato a Claude le scelte
  aperte della seconda review («decidi te»). Le scelte prese
  sono in «Seconda review (12/11)»; il #6 resta da decidere
  con Javier.
