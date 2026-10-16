# Graphify Rust Extractor

Samostatný štrukturálny extractor pre **PHP, SQL a HTML**. Vytvára `graph.json`,
ktorý vie načítať oficiálny Graphify. Je to prvá implementácia (0.1.0), nie úplný
port jeho jazykovej a LLM funkcionality.

## Použitie

```sh
cargo build --release --locked

./target/release/graphify-extractor extract /cesta/k/projektu \
  --out /cesta/k/vystupu \
  --exclude-file database/production.sql \
  --exclude 'backups/**' \
  --exclude-from excludes.txt \
  --sql-dialect auto
```

Bez `--out` sa použije `<projekt>/graphify-out`. Obsah iného existujúceho
`graph.json` v tomto adresári sa po úspešnom full-build **nahradí**; sémantická
vrstva starého grafu sa nemerguje. Na prvé skúšanie použi samostatný výstup.
Chyba parsovania, čítania, prekročenie limitu alebo chyba pred publikovaním ponechá
predchádzajúci graf nedotknutý a CLI skončí s nenulovým exit code.

Malý pripravený príklad:

```sh
./target/release/graphify-extractor extract examples/project --out graphify-out/demo
```

Stderr vypisuje počty súborov/uzlov/hrán, odvodených hrán, nevyriešených referencií, dynamických
PHP referencií a preskočených HTML/SQL konštrukcií. Rovnaké štatistiky sú uložené
v `graph.json.graph.stats`. Nevyriešená referencia nevytvára vymyslený cieľový uzol.

## Paralelné parsovanie

```sh
./target/release/graphify-extractor extract /cesta/k/projektu --workers 4
```

`--workers N` nastavuje počet súčasne parsovaných súborov; musí byť kladný.
Predvolené `1` zachováva najnižšiu spotrebu pamäte. V knižnici nastav
`Options::workers`. Viac workerov spotrebuje viac RAM na súbežné parsery;
nejde o pevný limit RSS. `--max-file-mib` naďalej limituje vstup PHP/HTML
alebo jednotlivý bufferovaný SQL statement, nie veľkosť AST v pamäti.

Pri paralelnom behu dostane každý worker najviac 32 ciest v jednej dávke
(a parsuje ich postupne). Vlákna aj ich dočasné SQLite databázy s 1 MiB page cache
sa používajú opakovane; fakty predchádzajúcej dávky sa po zlúčení vyčistia.
Naraz existuje najviac N dávok. `--parse-input-mib` (predvolene 64) navyše obmedzuje
súčet odhadovaných vstupných bajtov v jednej vlne. PHP/HTML sa účtuje podľa veľkosti
súboru, streamované SQL najviac do limitu bufferovaného statementu. Väčší samostatný
vstup sa môže spracovať osamote. Toto je limit prijatej práce, nie pevný limit RAM:
AST a SQLite môžu spotrebovať viac pamäte. Voľba sa uplatní pri viacerých workeroch.

Hlavná databáza zlúči dávky v pôvodnom poradí, aby sa nezmenila deduplikácia,
provenance ani výsledok rozlišovania symbolov. Resolver a export sú sériové.
Čakajúce výsledky zostávajú na disku; dočasné databázy sa odstránia po behu.

Chyba workera zruší publikovanie celého nového grafu; už spustené dávky sa
najprv dokončia a dočasné súbory sa odstránia. Existujúci graf zostáva zachovaný.

Výsledky meraní a kontrola nezmenených grafov/query: [paralelný Dolibarr benchmark](benchmarks/parallel/REPORT.md).

## Voliteľná cache parsovania

```sh
./target/release/graphify-extractor extract /cesta/k/projektu \
  --workers 8 --parse-input-mib 64 --cache-dir /cesta/k/cache-projektu
```

`--cache-dir` zapína diskovú cache surových faktov jednotlivých súborov. Použi
samostatný adresár; ak je v projekte, pri skenovaní sa vynechá. Koreň projektu ani
jeho predok nesmie byť cache adresárom. Bez tejto voľby sa persistentná cache nepoužíva.

Kľúč obsahuje hash obsahu, relatívnu cestu, parserové nastavenia a odtlačok kódu,
dependencies a gramatiky. Každý beh znovu enumeruje súbory a vyhodnotí exclusions,
načíta Composer kontext a vykoná celý resolver. Zmena autoload mapy alebo odstránenie
dependency preto prepočíta väzby aj pre nezmenený súbor načítaný z cache. Výstup je
stále úplný nový graf (`incremental: false` označuje tento režim publikovania);
cache nemení jeho uzly, hrany ani metadáta.

PHP/HTML sa hashujú z tých istých bajtov, ktoré sa parsujú; SQL sa streamovane
skopíruje do dočasného snapshotu. Obnova overí obsah cache pred importom faktov.
Poškodená alebo nedostupná cache sa obíde úplným parsovaním. Chyba po začatí importu
zastaví celý export, aby sa nezverejnil zmiešaný výsledok. `GRE_CACHE` na stderr
uvádza hits, misses, writes a errors oddelene od štatistík grafu.

Prvé naplnenie pridáva diskové zápisy a nemusí byť rýchlejšie než beh bez cache.
Staršie verzie obsahu zostávajú na disku; automatické čistenie zatiaľ nie je
implementované. Cache je obnoviteľná a jej samostatný adresár možno medzi behmi
odstrániť. Nejde o cache hotových query ani o vykonávanie projektového PHP.

## Exclusions

- Opakované `--exclude-file`: presné relatívne cesty, bez glob interpretácie.
- Opakované `--exclude`: globy voči koreňu projektu, vždy v úvodzovkách.
- Opakované `--exclude-from`: textové súbory s jedným globom na riadok.
- Automaticky sa načíta `.graphifyignore` v koreni projektu.
- `#` komentáre a prázdne riadky sa ignorujú. Neplatný glob/nečitateľný zoznam
  je chyba. Negácia `!` sa nepodporuje.
- `*` neprechádza cez `/`; `**` áno. `backups/` je skratka za `backups/**`.
  `**/*.dump.sql` pokrýva koreň aj podadresáre. Presná cesta `a.php` sa týka iba
  koreňa, na ľubovoľnú hĺbku použi `**/a.php`.
- Relatívna cesta k samotnému excludes súboru sa vyhodnocuje voči cwd; vzory v
  ňom vždy voči skenovanému projektu.
- `.gitignore` sa rešpektuje aj bez `.git` adresára; vypnutie cez `--no-gitignore`.
  Globálne gitignore pravidlá používateľa sa nepoužívajú. Symlinky sa nenasledujú.
- `.git` a vlastný výstupný adresár sa neprechádzajú. Exclusions sa aplikujú pred
  čítaním obsahu. Každý rebuild znovu vyhodnotí exclusions aj odstránené súbory.

```text
# excludes.txt: migrácie ostávajú, dátové exporty vynecháme
backups/**
database/dumps/**
**/*.dump.sql
```

## Čo sa extrahuje

| Vstup | Podporované |
| --- | --- |
| PHP | Tree-sitter AST: namespaces, class/interface/trait/enum, funkcie, metódy, closures, imports vrátane aliases/group use, explicitné function/static calls, odvodené receiver calls, `new`, extends/implements/trait-use, include cez `__DIR__`/`__FILE__` |
| SQL | CREATE TABLE a stĺpce, foreign keys, CREATE VIEW/INDEX, ALTER/DROP udalosti; prepájanie tabuliek podľa dialektu, kontextu a databázového názvu |
| HTML | Súborové odkazy, form actions, odkazy na skripty/styles/assets; len ciele prítomné v extrahovanom korpuse |
| PHP + HTML | HTML odkazy iba z doslovných template úsekov, nie z PHP stringov alebo komentárov |

PHP rozlišuje scope tried, takže `A::save` a `B::save` majú odlišné ID aj v
rovnakom súbore. Importy a volania sa prepájajú až po spracovaní celého projektu.
Resolver uprednostní konkrétnu enclosing triedu a jednoznačnú deklaráciu v tom istom
súbore; potom použije statický Composer kontext a presne kvalifikovaný symbol. Duplicitný stub preto nezruší
lokálne väzby. Medzi zostávajúcimi nejednoznačnými definíciami nevyberá náhodne. PHP syntax error alebo nepodporovaná syntax zastaví export.

`$this->method()` a volania cez parameter s jedným známym class/interface typom
sa prepájajú ako **INFERRED**, pretože runtime objekt môže byť podtrieda s override.
Heuristické `confidence_score: 0.7` nie je nameraná pravdepodobnosť. Cieľová metóda
sa vyhľadá podľa identity triedy: vlastná metóda, trait, potom predok. Cyklus,
konflikt alebo chýbajúci predok sa nepremostí odhadom podľa samotného mena. Typový hint sa nepoužije, ak parameter nie
je používaný výlučne ako receiver (napr. sa prepisuje alebo odovzdáva neznámemu
callee), je by-reference, variadic, union/intersection, alebo scope používa
`extract`, `eval` či dynamické premenné. `$this` sa nepoužije v statickom kontexte.
Ak rovnakú hranu dokazuje aj explicitné volanie, zachová sa `EXTRACTED` evidencia.

Jednoduché `$x = new C` poskytne receiver hint pre nasledujúce statementy.
Prepis, escape a neisté vetvenie ho zneplatnia; nové explicitné priradenie ho môže
obnoviť. `global $x` vo funkcii preberie známy hint z okolitého súborového scope
v mieste deklarácie; nejde o všeobecnú analýzu runtime globálnych premenných.
Správne top-level volania zostávajú hranami z file uzla.

`include/require(_once)` cez `__DIR__`/`__FILE__` a jednoduchú konkatenáciu
konštantných stringov sa prepájajú na zahrnuté súbory v koreni projektu. Cesty
mimo projektu, dynamické výrazy a bežný relatívny include sa nehádajú.

**Rozsah prvej verzie:** neznámy runtime receiver, late static binding,
include_path, framework routing, runtime `parent::` v zdieľaných traitoch, trait adaptations
a JavaScript sa sémanticky neriešia. Blade/Twig directive semantics sa neinterpretujú.

Explicitné `parent::method()` v triede sa vyhľadá od jej skutočného predka,
nie od override metódy dieťaťa. Nejednoznačné alebo chýbajúce dedenie sa nehádajú.
Composer classmap, PSR-4, PSR-0 a `autoload_files.php` metadáta sa čítajú staticky; PHP kód projektu sa nespúšťa. Funkcie sa vyberajú podľa presného FQN iba z jednoznačne registrovaných súborov. Vygenerované mapy majú prednosť pred samotným `autoload.files` vyhlásením vloženej knižnice; dve súperiace vygenerované mapy zostávajú nejednoznačné. Súperiace definície a registrované súbory s file-level `return` zostanú konzervatívne nerozhodnuté; vnorené funkčné návraty nie sú prekážkou. Nevykonávame podmienené include reťazce ani odhadovanie verzie PHP.
Dynamické zmeny autoloaderov a úplná runtime bootstrap sémantika nie sú simulované.

PHP identifikátory môžu obsahovať bajty mimo UTF-8. V grafe majú jednoznačný,
reverzibilný zápis: bajt `b` je `U+F100` nasledovaný `U+E000+b`, doslovný `U+F100`
sa zdvojí. Tým nevznikne kolízia s platným Unicode identifikátorom. Riadky ostávajú
nezmenené; interné byte offsety patria kanonicky zakódovanému vstupu.
Dlhé výrazové stromy sa prechádzajú iteratívne; limit hĺbky lexikálnych scopeov ostáva.

Súbory s úvodným `<?hh` sa zaevidujú ako `language=hack`, `_origin=inventory` a
započítajú do `unsupported_hack_files`. Ich symboly sa neextrahujú a neoznačujú
sa za úspešne parsované PHP. To nie je tiché vylúčenie súboru.

HTML sa parsuje tolerantne podľa HTML parsera. URL s `<base>`, percent-encoded
cesty, dynamické URL, externé URL a fragment-only odkazy sa preskočia a spočítajú.
Root-relative URL sa interpretujú voči koreňu projektu, nie podľa konfigurácie
webservera. Samostatné CSS/JS/obrázky sa neindexujú; odkazy na ne ostanú nevyriešené.
HTML hrany nemajú riadok, pretože DOM parser neposkytuje presný zdrojový offset.

## SQL dumpy a migrácie

Predvolený režim je `auto`: najbližší nadradený adresár `pgsql`, `postgres`,
`postgresql`, `mysql`, `mariadb`, `sqlite` alebo `sqlite3` určí dialekt súboru;
bez takého adresára sa použije MySQL. `auto` používa ANSI `--` komentáre aj bez
medzery (vrátane komentovaných stĺpcov a príkazov v projektových skriptoch).
Nejde o detekciu dialektu z ľubovoľného obsahu. Pre súbory mimo tejto adresárovej
konvencie alebo MySQL výrazy typu `4--1` použi explicitný `--sql-dialect mysql`,
`postgres`, `sqlite` alebo `generic`. Explicitný dialekt platí pre všetky SQL
súbory a MySQL zachováva pravidlo whitespace za `--`.
Lexer rešpektuje stringy, quoted identifikátory, komentáre, PostgreSQL dollar quotes,
MySQL `DELIMITER` a executable comments. MySQL executable comments sa analyzujú
bez simulácie konkrétnej server version podmienky. `ALTER TABLE ... ENABLE/DISABLE
KEYS` sa zachová ako migration event s väzbou na tabuľku. PostgreSQL
`CREATE LANGUAGE name` sa zachová ako language uzol, dollar-quoted telá funkcií
sa nerozdelia na vnútorných bodkočiarkach.

INSERT/REPLACE dáta sa lexikálne prejdú bez ukladania payloadu do RAM. PostgreSQL
textový `COPY ... FROM STDIN` sa preskočí až po `\.`. Binary COPY nie je podporované.
Toto je **schema-only režim**, nie validátor vložených dát ani vykonávač migrácií.
SET/transaction/data statements sa počítajú ako vynechané. Zmeny `search_path`
a `NO_BACKSLASH_ESCAPES`, ktoré by menili predpoklady parsera, sa odmietnu.
MySQL `USE database` a kvalifikované názvy sa zohľadňujú.
Podporované úplné tvary `CREATE USER ... IDENTIFIED ...`, `GRANT ALL ...` a
`FLUSH PRIVILEGES` sa validujú a započítajú do `ignored_sql_statements`, pretože
nemenia projekciu tabuliek. Nepodporované suffixy a poškodené tvary ostávajú chybou.

Graf zachytáva deklarácie a udalosti z migrácií, **nie konečný stav databázy po ich
vykonaní**. Duplicitné CREATE TABLE v rôznych dumpoch/migráciách ostávajú samostatné
definície. SQL resolver najprv vyhľadá jednoznačnú definíciu v tom istom súbore,
potom v rovnakom kontexte a dialekte uprednostní schému pred historickými CREATE
v adresároch `migration`/`migrations`. Ak schéma nemá definíciu, môže použiť
jednoznačnú historickú deklaráciu v tom istom kontexte. Konflikt medzi kandidátmi
rovnakej priority ostane nevyriešený; neexistuje fallback do cudzieho kontextu. Jednoznačný ALTER foreign key
má aj priamu table→table `references` hranu so zdrojovým súborom a riadkom migrácie;
pôvodný migration uzol a jeho hrany ostávajú zachované. Ďalšie príkazy, ktoré sqlparser
pozná, sa zachovajú ako `sql_statement` uzly, bez predstierania úplnej sémantiky.
Neznáma syntax je chyba s názvom súboru a približným počiatočným riadkom statementu.
View query dependencies, SQL v PHP stringoch a PHP migration framework DSL zatiaľ
nemajú samostatnú sémantickú analýzu.

SQL kontext je predvolene najbližší adresár pomenovaný podľa dialektu (napr.
`htdocs/install/mysql`); mimo tejto konvencie je to nadradený adresár súboru.
MySQL/MariaDB/PostgreSQL dump hlavičky v úvodných komentároch a MySQL
ENABLE/DISABLE KEYS automaticky izolujú snapshot do vlastného file kontextu.
Chýbajúci cieľ sa potom nikdy nepreberie z iného dumpu. Databázové kvalifikátory
aj MySQL `USE` zostávajú súčasťou identity; rôzne dialekty sa nemiešajú.

Pre inú štruktúru alebo rozdelený dump možno kontext nastaviť explicitne:

```sh
--sql-scope 'app/schema/**=app' --sql-scope 'app/migrations/**=app'
--sql-scope 'backups/**=@file'
```

`GLOB=NAME` spojí zodpovedajúce súbory do pomenovaného kontextu; `@file` každý
izoluje. Nejde o exclusions: všetky súbory sa spracujú. Explicitné pravidlo má
prednosť pred automatickou izoláciou dumpu. Rozporné pravidlá pre jeden súbor
sú chyba. Hranice kontextov a rola migrácií sú deklarované adresárové konvencie,
nie simulácia poradia SQL migrácií; v neštandardnom projekte ich nastav explicitne.
File uzly SQL exportujú `sql_scope`, `sql_dialect`, `sql_scope_reason`, `sql_role`
na kontrolu použitého rozlišovania.


## Pamäť, CPU a bezpečný export

```text
streamovaný walk → ohraničené dávky parserov → SQLite fakty
                → globálne SQL joins → streamovaný JSON → atomic rename
```

Nie je tu globálny `Vec<Node>` ani in-memory graph. SQLite page cache má 8 MiB,
temp operácie sú diskové a zápisy používajú pripravené statementy. Sekundárne
indexy sa vytvoria raz po zlúčení, až pred resolverom. Súkromné dočasné databázy
používajú `journal_mode=OFF` a `synchronous=OFF`: neobnovujú sa po páde a každá
chyba zahodí celý build. Toto nastavenie sa nepoužíva na používateľské databázy.
Finálny JSON sa naďalej flushne, synchronizuje a atomicky publikuje. Počet
parserových vlákien nastavuje `--workers`; resolver a export sú sériové.

`--max-file-mib` (default 16) obmedzuje PHP/HTML súbor a jeden **nedátový SQL
statement**, nie veľkosť celého SQL dumpu. PHP/HTML AST/DOM má dodatočnú pamäťovú
réžiu; tento parameter nie je tvrdý RSS limit. Nadlimitný súbor sa potichu nepreskočí.
Zvyšovanie limitu môže zvýšiť RAM. Disk musí mať miesto na dočasnú databázu aj JSON.

Výstup je deterministicky zoradený. ID používajú SHA-256 nad oddelenými komponentmi
cesty, druhu a kvalifikovaného názvu; PHP zahŕňa offset deklarácie. ID sú nezávislé
od absolútneho checkout adresára, ale presun/editácia deklarácie ich môže zmeniť.
Žiadne checkpointy sa nepublikujú pred úspechom; neexistuje problém zastaraného
change-indexu po zlyhanom builde. Prerušenie/pád môže zanechať dočasný adresár na disku.

## Graphify kompatibilita

Export je NetworkX node-link JSON: `directed: true`, `multigraph: true`, `nodes`
a `links`. Multigraph zachová napríklad `links_to` aj `submits_to` medzi rovnakými
súbormi. Uzly/hrany nesú `source_file`, `source_location`, `_origin` a hrany
`confidence`/`weight`. Hrany volaní/konštruktorov majú `context=call`,
importy/includes majú `context=import`, aby fungovali originálne query filtre.
Všetky endpointy sú deklarované.

Cieľom je **načítanie a query hotového grafu**, nie identita s výstupom Python
extractora. Vlastná ID schéma nie je kompatibilná s jeho merge/watch/cache protokolom.
Nepoužívaj oficiálny `extract/update` na tú istú mapu s očakávaním zachovania Rust
identít. Nepublikujeme Python manifest ani clustering/report artefakty. Oficiálny
reader stále načítava graf do RAM; úspora extrakcie neodstraňuje tento náklad.

## Testovanie

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

Fixtures pre end-to-end testy sa vytvárajú v dočasných adresároch. Overujú scope,
cross-file resolution, aliases, ambiguity, SQL dumping/quoting, HTML väzby,
exclusions, deterministickosť a zachovanie starej mapy po chybe.

Voliteľná kontrola skutočným upstream loaderom a query engine:

```sh
python scripts/check_graphify.py --upstream /cesta/ku/graphify \
  --graph /cesta/k/vystupu/graph.json --query example
```

Python potrebuje NetworkX a závislosti použitej upstream verzie. Nie sú potrebné
na spustenie Rust extractora. Pozri aj [VALIDATION.md](VALIDATION.md).

## Použitie ako Rust knižnica

```rust,no_run
use graphify_extractor::{Options, extract};

fn main() -> anyhow::Result<()> {
    let mut options = Options::new("/cesta/k/projektu");
    options.exclude.push("backups/**".into());
    options.exclude_file.push("database/production.sql".into());
    let stats = extract(&options)?;
    println!("{} uzlov, {} hrán", stats.nodes, stats.edges);
    Ok(())
}
```

## Porovnanie s originálom na Dolibarr

[Pôvodný benchmark](benchmarks/dolibarr/REPORT.md) zachytáva stav pred opravami
parserov, keď bolo potrebné vynechať 90 súborov.
[Aktuálne overenie celého korpusu](benchmarks/parser-fixes/REPORT.md) spracúva
všetkých 5 633 PHP/SQL/HTML súborov bez exclusions a obsahuje nové query porovnanie.

[Query porovnanie pred/po oprave context](benchmarks/dolibarr/QUERY_COMPARISON.md)
ukazuje rozdiely výsledkov cez originálny CLI aj MCP engine.

[Detailná analýza postupného pridávania dependency súborov](benchmarks/isolated-dolibarr-clean/ANALYSIS.md)
obsahuje nezmenené reálne vstupy, oba grafy, query odpovede a delty každého kroku.

[Overenie opráv lokálnych symbolov, receiverov a SQL projekcie](benchmarks/fixes-validation/REPORT.md)
obsahuje opakované izolované kroky aj veľký Dolibarr graf.

[Spracovanie celého Dolibarru po opravách parserov](benchmarks/parser-fixes/REPORT.md)
aktualizuje staršie výsledky s exclusions. PHP používa lokálne opravenú pripnutú
tree-sitter gramatiku; zmeny a regenerácia sú vo
[vendor/tree-sitter-php/PATCHES.md](vendor/tree-sitter-php/PATCHES.md).

[Overenie SQL kontextov na celom Dolibarre](benchmarks/sql-scopes/REPORT.md)
obsahuje presné obnovené endpointy, kontrolu izolácie dumpov a nové query výsledky.


## Profilovanie

`GRE_PROFILE=1` zapne diagnostické JSON udalosti na stderr: parse/merge okná,
jednotlivé súbory, podfázy resolvera a export. Bez premennej sa udalosti
nevytvárajú; diagnostika nemení obsah grafu. `scripts/profile_extract.py` ich
koreluje s RSS procesu pre pripnutý Dolibarr. RSS pri fáze nie je dôkazom, že
práve táto fáza vlastní všetku rezidentnú pamäť.

[Profil a optimalizácie SQLite/resolvera pri 8 workeroch](benchmarks/profile-bench/REPORT.md).

## Regresné testy a delegovanie

Každý opravovaný parserový alebo resolverový scenár musí mať trvalý test správnej
deklarácie/hrany a podľa potreby aj neprítomnosti nesprávneho cieľa. Pravidlá
a špecializácie agentov sú v [AGENTS.md](AGENTS.md). Základné kontroly:

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Veľké corpus/query porovnania dopĺňajú malé testy; nenahrádzajú ich. Zhoda s
Python Graphify sama osebe nedokazuje správnosť.

## Celé Magento, Moodle a Nextcloud s dependencies

[Nové overenie parsera a Composer resolvera](benchmarks/vendor-fixes-final-v2/REPORT.md)
obsahuje 101 testov, 36 sekvenčných meraní pri 8 workeroch a porovnanie oficiálnych
Graphify queries. Rust spracoval všetkých 110 999 vstupných súborov bez parserových
exclusions; tri Hack fixtures sú explicitne označené ako inventory bez extrakcie
symbolov. Všetkých 26 sledovaných cieľov hrán/query na celých projektoch je správnych;
nejde o tvrdenie o presnosti celého grafu. Report rozlišuje jazykové pokrytie Pythonu,
no-vendor diagnostiku a nutné zvýšenie query limitu pri veľkom Python grafe Moodle.
