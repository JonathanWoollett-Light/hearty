# hearty

[![Crates.io Version](https://img.shields.io/crates/v/hearty)](https://crates.io/crates/hearty)
[![Deps.rs Crate Dependencies (latest)](https://img.shields.io/deps-rs/hearty/latest)](https://crates.io/crates/hearty/0.1.2/dependencies)
[![Crates.io Size](https://img.shields.io/crates/size/hearty)](https://crates.io/crates/hearty/0.1.2)

Lints for hoi4 mods.

Most mods are very messy, this is a tool to help with a little cleaning.

The plan is to add more lints over time.

### GitHub Action

Can be used in a GitHub action with
```yaml
- name: Install hearty
    uses: baptiste0928/cargo-install@v3
    with:
        crate: hearty

- name: Run hearty lint
    run: hearty
```

### Usage

```text
Usage: hearty.exe [OPTIONS] [PATH]

Arguments:
  [PATH]  Path to the mod directory. Defaults to the current directory [default: .]

Options:
      --all          Check all languages
      --lang <LANG>  Languages to check. May be repeated: --lang english --lang german. Defaults to english if neither --lang nor --all is given [possible values: brazilian_portuguese, chinese, english, french, german, japanese, korean, polish, russian, spanish]
  -h, --help         Print help
```

### Example

![Lint warnings](https://raw.githubusercontent.com/JonathanWoollett-Light/hearty/refs/heads/master/image.png)
