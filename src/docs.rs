//! The hidden `--rules-json` flag: hearty's rules and options as JSON, read
//! from the code itself, for the rules site in `docs/`.
//!
//! `tests/docs.rs` builds the site's data from this JSON, the prose it holds
//! about each rule and examples it runs hearty on, and fails when the
//! committed site no longer matches: a redundant-field rule, block kind,
//! field order or option changed in the code must be regenerated into the
//! site (with `HEARTY_BLESS=1 cargo test --test docs`).

use crate::schema::{self, BlockKind, Redundant, RedundantRule};
use clap::CommandFactory as _;
use serde_json::{Value, json};

/// Every item of `items` in backticks, joined as a list in prose: `` `a`,
/// `b` or `c` ``.
fn code_list(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("`{item}`")).collect();
    match quoted.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => quoted.concat(),
    }
}

/// A block kind: its id and name, where its blocks are, its canonical field
/// order and the redundant-field rules that apply to it.
fn block_kind(kind: BlockKind) -> Value {
    let (id, name) = kind_names(kind);
    json!({
        "fields": kind.field_order(),
        "id": id,
        "location": location(kind),
        "name": name,
        "redundant_rules": schema::redundant_rules(kind)
            .iter()
            .map(|rule| rule_id(rule))
            .collect::<Vec<_>>(),
    })
}

/// The id (snake case, for anchors) and name (in prose) of `kind`.
const fn kind_names(kind: BlockKind) -> (&'static str, &'static str) {
    match kind {
        BlockKind::Advisor => ("advisor", "Advisor"),
        BlockKind::Character => ("character", "Character"),
        BlockKind::CorpsCommander => ("corps_commander", "Corps commander"),
        BlockKind::CountryLeader => ("country_leader", "Country leader"),
        BlockKind::Decision => ("decision", "Decision"),
        BlockKind::DecisionCategory => ("decision_category", "Decision category"),
        BlockKind::Event => ("event", "Event"),
        BlockKind::EventDesc => ("event_desc", "Event description"),
        BlockKind::EventOption => ("event_option", "Event option"),
        BlockKind::FieldMarshal => ("field_marshal", "Field marshal"),
        BlockKind::Focus => ("focus", "Focus"),
        BlockKind::FocusOffset => ("focus_offset", "Focus offset"),
        BlockKind::FocusTree => ("focus_tree", "Focus tree"),
        BlockKind::Idea => ("idea", "Idea"),
        BlockKind::NavyLeader => ("navy_leader", "Navy leader"),
        BlockKind::Scientist => ("scientist", "Scientist"),
        BlockKind::Technology => ("technology", "Technology"),
        BlockKind::TechnologyFolder => ("technology_folder", "Technology folder"),
        BlockKind::TechnologyPath => ("technology_path", "Technology path"),
    }
}

/// Where blocks of `kind` are, as [`schema::block_kind`] finds them, in
/// prose with code in backticks.
fn location(kind: BlockKind) -> String {
    let role = |role: &str| {
        format!(
            "`characters > <character> > {role}` in `common/characters/*.txt` (or in the \
             character's `instance`)"
        )
    };
    match kind {
        BlockKind::Advisor => role("advisor"),
        BlockKind::Character => "`characters > <character>` in `common/characters/*.txt`, and \
                                 its `instance` blocks"
            .to_owned(),
        BlockKind::CorpsCommander => role("corps_commander"),
        BlockKind::CountryLeader => role("country_leader"),
        BlockKind::Decision => {
            "`<category> > <decision>` in `common/decisions/*.txt` (missions too)".to_owned()
        }
        BlockKind::DecisionCategory => {
            "`<category>` in `common/decisions/categories/*.txt`".to_owned()
        }
        BlockKind::Event => format!(
            "A top-level {} in `events/*.txt`",
            code_list(schema::EVENT_TYPES)
        ),
        BlockKind::EventDesc => "An event's `desc` or `title` written as a block (`desc = { \
                                 text = .. trigger = { .. } }`) in `events/*.txt`"
            .to_owned(),
        BlockKind::EventOption => "An event's `option` in `events/*.txt`".to_owned(),
        BlockKind::FieldMarshal => role("field_marshal"),
        BlockKind::Focus => format!(
            "`focus_tree > focus` in `common/national_focus/*.txt`, and a top-level {}",
            code_list(schema::ROOT_FOCUS_KEYS)
        ),
        BlockKind::FocusOffset => "A focus's `offset` in `common/national_focus/*.txt`".to_owned(),
        BlockKind::FocusTree => "A top-level `focus_tree` in `common/national_focus/*.txt` (its \
                                 own fields: its focuses are sorted by focus sorting)"
            .to_owned(),
        BlockKind::Idea => "`ideas > <category> > <idea>` in `common/ideas/*.txt`".to_owned(),
        BlockKind::NavyLeader => role("navy_leader"),
        BlockKind::Scientist => role("scientist"),
        BlockKind::Technology => "`technologies > <technology>` in \
                                  `common/technologies/*.txt` (but not `@` variables)"
            .to_owned(),
        BlockKind::TechnologyFolder => {
            "A technology's `folder` in `common/technologies/*.txt`".to_owned()
        }
        BlockKind::TechnologyPath => {
            "A technology's `path` in `common/technologies/*.txt`".to_owned()
        }
    }
}

/// The options of `command` (built, so with `--help`), leaving out hidden
/// ones: what `--help` prints about each, and more.
fn options(command: &clap::Command) -> Vec<Value> {
    command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .map(|arg| {
            // A flag has a value too (`true` when given, else `false`), which
            // is no business of its reader.
            let takes_values = arg.get_action().takes_values();
            let values = |values: Vec<String>| if takes_values { values } else { Vec::new() };
            let possible_values = values(
                arg.get_possible_values()
                    .iter()
                    .filter(|value| !value.is_hide_set())
                    .map(|value| value.get_name().to_owned())
                    .collect(),
            );
            json!({
                "conflicts_with": command
                    .get_arg_conflicts_with(arg)
                    .iter()
                    .map(|other| other.get_id().as_str())
                    .collect::<Vec<_>>(),
                "default": values(
                    arg.get_default_values()
                        .iter()
                        .map(|value| value.to_string_lossy().into_owned())
                        .collect()
                ),
                "help": arg
                    .get_long_help()
                    .or_else(|| arg.get_help())
                    .map(ToString::to_string),
                "id": arg.get_id().as_str(),
                "long": arg.get_long(),
                "positional": arg.is_positional(),
                "possible_values": possible_values,
                "repeatable": matches!(arg.get_action(), clap::ArgAction::Append),
                "short": arg.get_short().map(String::from),
                "value_names": values(
                    arg.get_value_names()
                        .unwrap_or_default()
                        .iter()
                        .map(ToString::to_string)
                        .collect()
                ),
            })
        })
        .collect()
}

/// A redundant-field rule: its id, the field, the kinds of block it applies
/// to and why the field has no effect.
fn redundant_rule(rule: &RedundantRule) -> Value {
    let (kind, value) = match rule.value {
        Redundant::Block(pairs) => {
            let pairs: Vec<String> = pairs
                .iter()
                .map(|(key, value)| format!("{key} = {value}"))
                .collect();
            ("block", Some(format!("{{ {} }}", pairs.join(" "))))
        }
        Redundant::EmptyBlock => ("empty_block", Some("{ }".to_owned())),
        Redundant::OwnName => ("own_name", None),
        Redundant::Scalar(value) => ("scalar", Some(value.to_owned())),
    };
    json!({
        "explanation": rule.explanation,
        "id": rule_id(rule),
        "key": rule.key,
        "kinds": rule
            .kinds
            .iter()
            .map(|&kind| kind_names(kind).0)
            .collect::<Vec<_>>(),
        "repeatable": rule.repeatable,
        "value": value,
        "value_kind": kind,
    })
}

/// A stable id for `rule`, made of `redundant_`, its key and its value, e.g.
/// `redundant_fire_only_once_no`, `redundant_offset_x_0_y_0` or
/// `redundant_available_empty`: lowercase letters, digits and `_`.
fn rule_id(rule: &RedundantRule) -> String {
    let value = match rule.value {
        Redundant::Block(pairs) => pairs
            .iter()
            .flat_map(|&(key, value)| [key, value])
            .collect::<Vec<_>>()
            .join("_"),
        Redundant::EmptyBlock => "empty".to_owned(),
        Redundant::OwnName => "own_name".to_owned(),
        Redundant::Scalar(value) => value.to_owned(),
    };
    let mut id = String::new();
    for ch in format!("redundant_{}_{value}", rule.key).chars() {
        if ch.is_ascii_alphanumeric() {
            id.push(ch.to_ascii_lowercase());
        } else if !id.is_empty() && !id.ends_with('_') {
            id.push('_');
        }
    }
    id.trim_end_matches('_').to_owned()
}

/// The rules and options as pretty-printed JSON (see the module docs): the
/// block kinds, the redundant-field rules, the options, the environment
/// variables hearty reads, and constants the site's prose refers to.
pub fn rules_json() -> String {
    let mut command = crate::Args::command();
    command.build();
    let document = json!({
        "block_kinds": BlockKind::ALL.map(block_kind),
        "constants": {
            "max_item_digits": crate::reflow::MAX_ITEM_DIGITS,
            "max_marker": crate::reflow::MAX_MARKER,
            "max_shown": crate::MAX_MISSING,
            "min_text_columns": crate::reflow::MIN_TEXT_COLUMNS,
            "tab_width": crate::inline::TAB_WIDTH,
            "unwrapped_margin": crate::reflow::UNWRAPPED_MARGIN,
            "unwrapped_width": crate::reflow::UNWRAPPED_WIDTH,
        },
        "environment": [
            { "name": crate::game::GAME_DIR_VAR, "option": "game_dir" },
            { "default": crate::version::DEFAULT_CACHE_DIR, "name": crate::version::CACHE_DIR_VAR },
            { "name": crate::FORCE_COLOR_VAR },
            { "name": crate::NO_COLOR_VAR },
        ],
        "options": options(&command),
        "redundant_rules": schema::REDUNDANT_RULES
            .iter()
            .map(redundant_rule)
            .collect::<Vec<_>>(),
    });
    let mut out = serde_json::to_string_pretty(&document).unwrap_or_default();
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::{code_list, rule_id, rules_json};
    use crate::schema::{BlockKind, REDUNDANT_RULES};
    use std::collections::HashSet;

    #[test]
    fn rule_ids_are_unique_and_plain() {
        let ids: Vec<String> = REDUNDANT_RULES.iter().map(rule_id).collect();
        let unique: HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        for id in &ids {
            assert!(
                id.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
                "{id}"
            );
            assert!(!id.contains("__") && !id.ends_with('_'), "{id}");
        }
        assert!(ids.contains(&"redundant_fire_only_once_no".to_owned()));
        assert!(ids.contains(&"redundant_offset_x_0_y_0".to_owned()));
        assert!(ids.contains(&"redundant_available_empty".to_owned()));
        assert!(ids.contains(&"redundant_picture_own_name".to_owned()));
    }

    #[test]
    fn lists_read_as_prose() {
        assert_eq!(code_list(&[]), "");
        assert_eq!(code_list(&["a"]), "`a`");
        assert_eq!(code_list(&["a", "b", "c"]), "`a`, `b` or `c`");
    }

    /// The JSON holds every rule, kind and visible option.
    #[test]
    fn json_holds_everything() {
        let json: serde_json::Value =
            serde_json::from_str(&rules_json()).unwrap_or(serde_json::Value::Null);
        let count = |key: &str| {
            json.get(key)
                .and_then(|items| items.as_array())
                .map(Vec::len)
        };
        assert_eq!(count("redundant_rules"), Some(REDUNDANT_RULES.len()));
        assert_eq!(count("block_kinds"), Some(BlockKind::ALL.len()));
        let options: Vec<&str> = json
            .get("options")
            .and_then(|options| options.as_array())
            .into_iter()
            .flatten()
            .filter_map(|option| option.get("id")?.as_str())
            .collect();
        assert!(options.contains(&"max_width") && options.contains(&"help"));
        assert!(!options.contains(&"rules_json"), "hidden");
    }
}
