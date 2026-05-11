# hearty

[![Crates.io Version](https://img.shields.io/crates/v/hearty)](https://crates.io/crates/hearty)
[![Deps.rs Crate Dependencies (latest)](https://img.shields.io/deps-rs/hearty/latest)](https://crates.io/crates/hearty/dependencies)
[![Crates.io Size](https://img.shields.io/crates/size/hearty)](https://crates.io/crates/hearty)

Lints and formatting for Hearts of Iron 4 mods.

Most mods are very messy, this is a tool to help with a little cleaning.

The plan is to add more lints over time.

### Installation

#### Linux 

```bash
curl -L https://github.com/JonathanWoollett-Light/hearty/releases/latest/download/hearty-x86_64-linux.tar.gz | tar xz
./hearty /path/to/mod
```

#### Windows (PowerShell)
```powershell
Invoke-WebRequest https://github.com/JonathanWoollett-Light/hearty/releases/latest/download/hearty-x86_64-windows.zip -OutFile hearty.zip
Expand-Archive hearty.zip -DestinationPath .
.\hearty.exe "C:\path\to\mod"
```

#### Source

```bash
cargo install hearty
hearty /path/to/mod
```

### GitHub Action

Can be used in a GitHub action with
```yaml
- name: Install hearty
    uses: baptiste0928/cargo-install@v3
    with:
        crate: hearty

- name: Cache HOI4 data
  uses: actions/cache@v4
  with:
    path: .hearty-cache
    key: hoi4-version-cache

- name: Check lints and formatting
    run: hearty --lint --check
```

### Usage

```text
Usage: hearty.exe [OPTIONS] [PATH]

Arguments:
  [PATH]  Path to the mod directory. Defaults to the current directory [default: .]

Options:
      --all          Check all languages
      --lang <LANG>  Languages to check. May be repeated: --lang english --lang german. Defaults to english if neither --lang nor --all is given [possible values: brazilian_portuguese, chinese, english, french, german, japanese, korean, polish, russian, spanish]
      --lint         Run localisation/version checks. Enabled by default when no action flag is given
      --format       Reorder focus blocks in national_focus files in place
      --check        Verify focus-block ordering without modifying files; exits non-zero on drift
  -h, --help         Print help
```

### Example

![Lint warnings](https://raw.githubusercontent.com/JonathanWoollett-Light/hearty/refs/heads/master/image.png)

### Todo

In no particular order.

- Extend focus sorting functionality to events (events should be ordered based on event chains, then alphabetically).
- Add more basic formatting e.g. `prerequisite = { focus = my_focus }` should be 1 line, and should have this exact spacing.
- Check events can be fired (sometimes old events end up existing in code but never being used, just being clutter).
- Check focuses, events, etc. have gfx.
- Check localisation spelling and grammar.