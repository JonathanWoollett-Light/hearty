# Developing hearty

What the formatter and the lints do is on the [rules site](https://jonathanwoollett-light.github.io/hearty/). This page covers working on hearty itself: [testing](#testing), [profiling](#profiling), [benchmarking](#benchmarking) and the [roadmap](#roadmap).

## Testing

`.github/workflows/ci.yml` runs these jobs on every pull request and every push to master. Each runs locally too:

- **CI:** `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- --deny warnings`, `RUSTDOCFLAGS='--deny warnings' cargo doc --no-deps --document-private-items` and `cargo test`. `src/main.rs` forbids `unsafe` code and turns on `clippy::pedantic` and `clippy::restriction`, less an allow-list, for the binary and its unit tests (not `tests/` or `benches/`). CI uses the latest stable Rust, and each release can add restriction lints, so `rustup update` before linting.
- **MSRV build:** the binary builds with the `rust-version` in `Cargo.toml`: `cargo +1.88 build --locked`. The tests may need a newer Rust (1.91).
- **Dependencies:** `cargo deny check` checks for known vulnerabilities, licences, duplicate or wildcard versions and where crates come from, as set in `deny.toml`, and `cargo machete` for unused dependencies. `audit.yml` also checks for new advisories every week.
- **Workflows:** `actionlint` and `zizmor .` check the workflows and `dependabot.yml`. Actions are pinned to commit SHAs, which Dependabot updates. The exception is actionlint's own Docker image in `ci.yml`, pinned by digest: update it by hand with a newer tag's digest from Docker Hub.
- **Markdown:** `npx markdownlint-cli2`, with the rules and files in `.markdownlint-cli2.yaml`.

The tools install with `cargo install --locked cargo-deny cargo-machete zizmor`, and actionlint from its [releases](https://github.com/rhysd/actionlint/releases). `commitlint.yml` checks each commit message is a [conventional commit](https://www.conventionalcommits.org/), from which release-plz writes the changelog.

`cargo test` runs each module's unit tests, `tests/docs.rs`, which checks the [rules site](#rules-site), and `tests/integration_tests.rs`, which runs the binary on copies of `tests/test_mod`: a small mod with a file of every kind hearty handles, whose comments say what each file is for.

- **Offline.** No test reaches the network. `--lint` checks `descriptor.mod` against HOI4's newest version, so every lint run gets a fresh version cache in a temporary `HEARTY_CACHE_DIR` or a fake steamcmd, and hearty's HTTP requests go to a proxy on a closed local port, so a download started by mistake fails instead of leaving the machine. `HEARTY_GAME_DIR` is set empty so an installed copy of the game isn't read; `tests/fake_game` stands in for it where a test needs one.
- **Golden hashes.** `test_fmt` formats `tests/test_mod` and compares a hash of the directory with `HASH`. Git checks the fixtures out with CRLF line endings on Windows (`core.autocrlf`) and LF elsewhere, so there is one hash for each; `.gitattributes` pins a few fixtures' line endings so both kinds of file are tested everywhere. When a change to the formatter or the fixtures changes the output on purpose, update both: run the test on Windows, and on Linux or WSL in a checkout with LF line endings, and take each new hash from the failed assertion. Such a change usually changes the rules site's examples too: regenerate its data as below.
- **Benchmark support.** `tests/bench_checkout.rs` tests the Millennium Dawn benchmark's clone and fetch against a small local repository, and `tests/bench_workshop.rs` tests reading hearty's output, finding Steam libraries and syncing a mod. Both need git.

### Rules site

The [rules site](https://jonathanwoollett-light.github.io/hearty/) is the plain HTML, CSS and JavaScript in `docs/` (`index.html`, `style.css`, `app.js`), which show the data in `docs/rules.js`. That data is generated, never edited by hand: `tests/docs.rs` builds it from the hidden `hearty --rules-json` (the redundant-field rules, the block kinds and their field orders, the options and the environment variables, read from the code), the prose it holds about the other rules, and examples it runs hearty on. Its test `rules_site_is_up_to_date` fails when the committed `docs/rules.js` differs from what it builds, or when a link on the site, or from this page or the README into it, leads nowhere; `every_lint_and_change_is_documented` fails when a lint diagnostic or a kind of formatting change in `src/` has no rule describing it.

After changing a rule, an option, `--help`, or anything hearty prints or writes for an example, regenerate the data and commit it:

```bash
HEARTY_BLESS=1 cargo test --test docs
```

`.github/workflows/pages.yml` publishes `docs/` when a push to master changes it. It needs **Settings → Pages → Build and deployment → Source** set to "GitHub Actions", once.

### Corpus tests

The `#[ignore]`d tests run hearty over whole mods, in memory, and print statistics. They check that the parser is lossless and matches [jomini](https://github.com/rakaly/jomini); that sorting, field order, joining blocks, reflowing comments and `--fix` keep every token, still parse and change nothing the second time; and that the lines added and removed in the report add up. Point `HEARTY_CORPUS` at `;`-separated HOI4 or mod folders (they're only read) and run them in release mode, all at once or by name:

```bash
HEARTY_CORPUS='C:\Program Files (x86)\Steam\steamapps\common\Hearts of Iron IV;C:\path\to\mod' cargo test --release -- --ignored --nocapture
HEARTY_CORPUS='...' cargo test --release corpus_sort -- --ignored --nocapture
```

The reflow test prints sample hunks; `HEARTY_REFLOW_WIDTH` sets its width (default 100), `HEARTY_REFLOW_DUMP=<file>` writes every hunk to a file, and `HEARTY_REFLOW_FROZEN=<file>` every paragraph left too wide. `parse_throughput` times the parser against jomini over `HEARTY_BENCH_RUNS` passes (default 11).

### Coverage

With [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) (`cargo install cargo-llvm-cov`):

```bash
cargo llvm-cov --open
```

## Profiling

`--timings` prints where a run's time went to stderr, and `--flamegraph FILE` draws the same as an SVG flamegraph (open it in a browser: hover over a frame for its time, click to zoom). They work with any actions and don't change what the actions do. `hearty --check --lint --timings` on Millennium Dawn:

```text
Timings: 592.95ms wall, 11.44s active on 26 threads (rayon pool: 24), parallelism 19.28x
  active: time threads spent in a span and the spans under it, summed over threads
  self:   active time not spent in a span nested in it on the same thread
  share:  active time as a share of the total; wall: first to last moment of a top-level part

span                     active      self  calls  share      wall
scripts                  11.20s    0.00ns      1  97.9%  569.74ms
  file                   11.20s   62.58ms  6,457  97.9%
    check                10.23s  111.38ms  6,457  89.4%
      line stats          8.29s     8.29s  4,300  72.5%
      inline           763.08ms  386.68ms  6,457   6.6%
        parse          376.40ms  376.40ms  4,106   3.2%
      parse            521.15ms  521.15ms  6,647   4.5%
      field order      406.98ms  190.06ms  6,457   3.5%
        parse          216.92ms  216.92ms    737   1.8%
      reflow comments   90.97ms   90.97ms  6,457   0.7%
      … 2 more          45.55ms                    0.3%
    read               757.56ms  757.56ms  6,457   6.6%
    lint               142.84ms   10.36ms  6,457   1.2%
      redundant find   125.80ms  125.80ms  6,457   1.1%
      … 1 more           6.68ms                    0.0%
localisation load      141.45ms    0.00ns      1   1.2%  569.74ms
  file                 141.45ms    2.47ms    503   1.2%
    keys                85.21ms   85.21ms    503   0.7%
    … 1 more            53.78ms                    0.4%
report                  50.71ms   19.30µs      1   0.4%   11.53ms
  … 4 more              50.69ms                    0.4%
find files              43.61ms    0.00ns      1   0.3%    8.78ms
  … 1 more              43.61ms                    0.3%
version check            2.85ms  103.60µs      1   0.0%    2.85ms
  … 2 more               2.74ms                    0.0%
parse                   21.90µs   21.90µs      1   0.0%   29.50µs

Slowest files:
  localisation load > file
       29.50ms  localisation/english/MD_focus_FRA_l_english.yml
        2.18ms  [game]/localisation/english/wuw_events_l_english.yml
        2.17ms  localisation/english/MD_politics_view_parties_l_english.yml
        1.78ms  localisation/english/equipment_l_english.yml
        1.45ms  localisation/english/MD_techs_l_english.yml
  scripts > file
      407.89ms  common/national_focus/05_russia.txt
      227.45ms  common/national_focus/05_saudi_arabia.txt
      222.60ms  common/national_focus/05_usa.txt
      218.65ms  common/national_focus/05_algeria.txt
      213.32ms  events/Iran.txt
```

- **active** is the time threads spent in a part, including the parts under it, summed over threads. Files are formatted and linted on every core at once, so a part's active time can be many times the run's wall time. A thread waiting for others counts nothing.
- **self** is active time not spent in a part nested in it on the same thread.
- **share** is the part's share of the total active time. Parts under 0.5% are folded into a `… N more` line.
- **wall** is the time from the start to the end of a top-level part: finding the files, the pass over the script files (each file is read, fixed, formatted, written, checked and linted in turn), loading the localisation (alongside that pass), the version check (on a thread of its own) and printing the report.
- **parallelism** is total active time over wall time: how many threads were busy on average.

The flamegraph shows the same tree: a frame's width is its active time.

hearty uses the [mimalloc](https://github.com/microsoft/mimalloc) allocator, which is much faster than the system allocator when every core allocates at once, but holds on to more memory: on Millennium Dawn or vanilla HOI4, `--check` and `--format` peak at 0.3–0.7 GB of RAM (up to 0.9 GB committed), and `--lint` at 0.15–0.35 GB.

## Benchmarking

### Millennium Dawn

`cargo bench --bench millennium_dawn` benchmarks hearty on the `main` branch of [Millennium Dawn](https://github.com/MillenniumDawn/Millennium-Dawn), a large mod.

- The first run clones the mod into `target/tmp/Millennium-Dawn` (set `HEARTY_BENCH_DIR` to clone somewhere else). The clone is shallow, blobless and sparse (`common`, `events`, `history`, `localisation` and the root files): about 30 seconds and 470 MiB. Later runs fetch `main` and reset to it, so they only download what changed. The clone is made beside its folder and moved into place once complete, so a failed or interrupted clone is started again next time, and the benchmark only resets a clone it made, so pointing it at your own checkout fails instead of discarding changes.
- It runs `--check --lint`, `--fix --format`, then each of `--lint`, `--check`, `--fix` and `--format` on its own: once to warm up, then 3 timed runs, and prints the fastest, median and slowest. Runs that change files are undone with `git reset --hard` and `git clean`, and then every file is read once, as on Windows the first read of a file after it's written waits for the antivirus to scan it.
- `--check --lint` and `--fix --format` then run once more with `--timings` and `--flamegraph`: their breakdowns are printed and their flamegraphs written to `target/tmp/flamegraph-check-lint.svg` and `target/tmp/flamegraph-fix-format.svg`.
- The HOI4 version cache is kept in `target/tmp/hearty-cache`, so steamcmd only runs on the first warm-up of the day.

### Steam Workshop mods

`cargo bench --bench workshop_mods` runs the same scenarios on five large Steam Workshop mods: [The Road to 56](https://steamcommunity.com/sharedfiles/filedetails/?id=820260968), [Kaiserreich](https://steamcommunity.com/sharedfiles/filedetails/?id=1521695605), [Old World Blues](https://steamcommunity.com/sharedfiles/filedetails/?id=2265420196), [The Fire Rises](https://steamcommunity.com/sharedfiles/filedetails/?id=3350890356) and [Millennium Dawn](https://steamcommunity.com/sharedfiles/filedetails/?id=2777392649) (its Workshop release). Set `HEARTY_BENCH_MODS` to a comma-separated list of slugs (`road-to-56`, `kaiserreich`, `old-world-blues`, `the-fire-rises`, `millennium-dawn`) or Workshop ids to run only some.

- **Getting the mods.** HOI4 isn't free, so steamcmd can't download its Workshop items anonymously. Each mod is taken from the first of:
  1. the folder in `HEARTY_BENCH_MOD_<id>`, e.g. `HEARTY_BENCH_MOD_1521695605`;
  2. the Workshop folder of any Steam library (`steamapps/workshop/content/394360/<id>`), so subscribing to a mod is enough. Steam is found in its default locations and, on Windows, the registry; set `HEARTY_BENCH_STEAM_DIR` to add another installation;
  3. a steamcmd download, when `HEARTY_BENCH_STEAM_USER` names a Steam account that owns HOI4 and steamcmd already holds its login (run `steamcmd +login <user> +quit` once). Set `HEARTY_BENCH_STEAMCMD` if steamcmd isn't on `PATH`.

  A mod found in none of these is skipped, with a message saying how to provide it.
- **Never touching Steam's copy.** The files hearty reads (`descriptor.mod`, `common`, `events`, `history` and `localisation`) are synced byte for byte into a git working copy in `target/tmp/workshop-mods/<slug>` (set `HEARTY_BENCH_WORKSHOP_DIR` to move it). Runs that change files are undone with `git reset`, and later benchmark runs only copy the files the mod has changed.
- **Results.** Everything goes to `target/tmp/workshop-mods/results.json`: each scenario's timings, the `--timings` breakdowns, and the problems hearty found (missing localisations, redundant fields, files `--format` would change and how, and `descriptor.mod` warnings). The flamegraphs go next to it.

### Charts

`python scripts/plot_benchmarks.py` (needs `pip install matplotlib`) draws 1920x1080 charts of the results in `target/tmp/workshop-mods/charts/`: `hero`, `problems` and `speed` (the README's and the two below), and `execution_time`, `formatting_changes` and `time_breakdown` in more detail. Options: another `results.json`, `--out DIR`, `--theme dark|light`, `--format png|svg|pdf`, and `--only CHART ...`.

The README's charts live in `docs/charts`, one per theme. To redraw them after a benchmark run:

```bash
python scripts/plot_benchmarks.py --out docs/charts/dark --only hero problems speed
python scripts/plot_benchmarks.py --out docs/charts/light --theme light --only hero problems speed
```

<p><picture><source media="(prefers-color-scheme: dark)" srcset="charts/dark/problems.png"><img alt="What hearty finds in each mod" src="charts/light/problems.png" width="49%"></picture> <picture><source media="(prefers-color-scheme: dark)" srcset="charts/dark/speed.png"><img alt="How long hearty takes on each mod" src="charts/light/speed.png" width="49%"></picture></p>

## Roadmap

In no particular order.

- Check events can be fired: old events are sometimes left in a mod that nothing fires any more.
- Check focuses, events and so on have their sprites (`GFX_...`).
- Check localisation spelling and grammar.
- Lint events that come before an event firing them, which `--format` can't fix when the file can't be sorted (e.g. its events fire each other in a cycle).
