//! HOI4 domain knowledge shared by the formatter and linter: which files hold
//! which kinds of definition blocks, the canonical order of each kind's
//! fields, and which field values are redundant (equal to the game's default
//! or otherwise without effect).
//!
//! Field orders were mined from vanilla HOI4 (primary authority) and three
//! large mods: pairwise key precedence was aggregated per kind and linearised,
//! with genuinely contested pairs settled by a reading order (identity →
//! position → cost → prerequisites → gates → modifiers → ai → effects).
//! Redundant values are limited to documented defaults with no known side
//! effect of writing them explicitly.
use std::collections::HashMap;
use std::path::{Component, Path};
use std::sync::LazyLock;

/// HOI4 event block types.
pub const EVENT_TYPES: &[&str] = &[
    "country_event",
    "news_event",
    "operative_leader_event",
    "state_event",
    "unit_leader_event",
];

/// Top-level keys under which a focus definition can appear in a national
/// focus file (besides `focus_tree > focus`).
pub const ROOT_FOCUS_KEYS: &[&str] = &["joint_focus", "shared_focus"];

/// Every redundant-field rule. See [`redundant_rules`].
pub const REDUNDANT_RULES: &[RedundantRule] = &[
    // --- Scalars set to their default -------------------------------------
    RedundantRule {
        explanation: "`fire_only_once` defaults to `no`",
        key: "fire_only_once",
        kinds: &[BlockKind::Decision, BlockKind::Event],
        repeatable: false,
        value: Redundant::Scalar("no"),
    },
    RedundantRule {
        explanation: "`is_triggered_only` defaults to `no`",
        key: "is_triggered_only",
        kinds: &[BlockKind::Event],
        repeatable: false,
        value: Redundant::Scalar("no"),
    },
    RedundantRule {
        explanation: "`hidden` defaults to `no`",
        key: "hidden",
        kinds: &[BlockKind::Event],
        repeatable: false,
        value: Redundant::Scalar("no"),
    },
    RedundantRule {
        explanation: "`major` defaults to `no`",
        key: "major",
        kinds: &[BlockKind::Event],
        repeatable: false,
        value: Redundant::Scalar("no"),
    },
    RedundantRule {
        explanation: "`available_if_capitulated` defaults to `no`",
        key: "available_if_capitulated",
        kinds: &[BlockKind::Focus],
        repeatable: false,
        value: Redundant::Scalar("no"),
    },
    RedundantRule {
        explanation: "`cancel_if_invalid` defaults to `yes` for focuses",
        key: "cancel_if_invalid",
        kinds: &[BlockKind::Focus],
        repeatable: false,
        value: Redundant::Scalar("yes"),
    },
    RedundantRule {
        explanation: "`continue_if_invalid` defaults to `no`",
        key: "continue_if_invalid",
        kinds: &[BlockKind::Focus],
        repeatable: false,
        value: Redundant::Scalar("no"),
    },
    RedundantRule {
        explanation: "`visible_when_empty` defaults to `no`",
        key: "visible_when_empty",
        kinds: &[BlockKind::DecisionCategory],
        repeatable: false,
        value: Redundant::Scalar("no"),
    },
    RedundantRule {
        explanation: "`cancel_if_not_visible` defaults to `no`",
        key: "cancel_if_not_visible",
        kinds: &[BlockKind::Decision],
        repeatable: false,
        value: Redundant::Scalar("no"),
    },
    RedundantRule {
        explanation: "`is_good` defaults to `no`",
        key: "is_good",
        kinds: &[BlockKind::Decision],
        repeatable: false,
        value: Redundant::Scalar("no"),
    },
    RedundantRule {
        explanation: "`selectable_mission` defaults to `no`",
        key: "selectable_mission",
        kinds: &[BlockKind::Decision],
        repeatable: false,
        value: Redundant::Scalar("no"),
    },
    RedundantRule {
        explanation: "a focus tree is only the default tree with `default = yes`",
        key: "default",
        kinds: &[BlockKind::FocusTree],
        repeatable: false,
        value: Redundant::Scalar("no"),
    },
    RedundantRule {
        explanation: "the continuous focus palette defaults to `x = 50 y = 1000`",
        key: "continuous_focus_position",
        kinds: &[BlockKind::FocusTree],
        repeatable: false,
        value: Redundant::Block(&[("x", "50"), ("y", "1000")]),
    },
    RedundantRule {
        explanation: "`can_be_captured` defaults to `yes`",
        key: "can_be_captured",
        kinds: &[BlockKind::Character],
        repeatable: false,
        value: Redundant::Scalar("yes"),
    },
    RedundantRule {
        explanation: "an idea already uses the sprite `GFX_idea_<its name>`",
        key: "picture",
        kinds: &[BlockKind::Idea],
        repeatable: false,
        value: Redundant::OwnName,
    },
    // --- Trigger blocks equivalent to omitting them ------------------------
    RedundantRule {
        explanation: "`allowed_civil_war` is already never true when omitted",
        key: "allowed_civil_war",
        kinds: &[BlockKind::Idea],
        repeatable: false,
        value: Redundant::Block(&[("always", "no")]),
    },
    RedundantRule {
        explanation: "an empty `allowed` is always true, the same as omitting it",
        key: "allowed",
        kinds: &[
            BlockKind::Advisor,
            BlockKind::Decision,
            BlockKind::DecisionCategory,
            BlockKind::Idea,
        ],
        repeatable: false,
        value: Redundant::EmptyBlock,
    },
    RedundantRule {
        explanation: "an omitted `allowed` is already always true",
        key: "allowed",
        kinds: &[
            BlockKind::Advisor,
            BlockKind::Decision,
            BlockKind::DecisionCategory,
            BlockKind::Idea,
        ],
        repeatable: false,
        value: Redundant::Block(&[("always", "yes")]),
    },
    RedundantRule {
        explanation: "an empty `visible` is always true, the same as omitting it",
        key: "visible",
        kinds: &[
            BlockKind::Advisor,
            BlockKind::CorpsCommander,
            BlockKind::CountryLeader,
            BlockKind::Decision,
            BlockKind::DecisionCategory,
            BlockKind::FieldMarshal,
            BlockKind::Idea,
            BlockKind::NavyLeader,
            BlockKind::Scientist,
        ],
        repeatable: false,
        value: Redundant::EmptyBlock,
    },
    RedundantRule {
        explanation: "an omitted `visible` is already always true",
        key: "visible",
        kinds: &[
            BlockKind::Advisor,
            BlockKind::CorpsCommander,
            BlockKind::CountryLeader,
            BlockKind::Decision,
            BlockKind::DecisionCategory,
            BlockKind::FieldMarshal,
            BlockKind::Idea,
            BlockKind::NavyLeader,
            BlockKind::Scientist,
        ],
        repeatable: false,
        value: Redundant::Block(&[("always", "yes")]),
    },
    RedundantRule {
        explanation: "an empty `available` is always true, the same as omitting it",
        key: "available",
        kinds: &[
            BlockKind::Advisor,
            BlockKind::Character,
            BlockKind::Decision,
            BlockKind::DecisionCategory,
            BlockKind::Focus,
            BlockKind::Idea,
        ],
        repeatable: false,
        value: Redundant::EmptyBlock,
    },
    RedundantRule {
        explanation: "an empty `trigger` imposes no condition",
        key: "trigger",
        kinds: &[BlockKind::Event, BlockKind::EventOption],
        repeatable: false,
        value: Redundant::EmptyBlock,
    },
    RedundantRule {
        explanation: "a `trigger` that is always true imposes no condition",
        key: "trigger",
        kinds: &[BlockKind::Event, BlockKind::EventOption],
        repeatable: false,
        value: Redundant::Block(&[("always", "yes")]),
    },
    RedundantRule {
        explanation: "an empty `allow_branch` is always true, the same as omitting it",
        key: "allow_branch",
        kinds: &[BlockKind::Focus],
        repeatable: false,
        value: Redundant::EmptyBlock,
    },
    RedundantRule {
        explanation: "an omitted `allow_branch` is already always true",
        key: "allow_branch",
        kinds: &[BlockKind::Focus],
        repeatable: false,
        value: Redundant::Block(&[("always", "yes")]),
    },
    RedundantRule {
        explanation: "a `cancel` that is never true never removes the idea",
        key: "cancel",
        kinds: &[BlockKind::Idea],
        repeatable: false,
        value: Redundant::Block(&[("always", "no")]),
    },
    // --- Blocks that do nothing ---------------------------------------------
    RedundantRule {
        explanation: "an empty `mutually_exclusive` excludes nothing",
        key: "mutually_exclusive",
        kinds: &[BlockKind::Focus],
        repeatable: true,
        value: Redundant::EmptyBlock,
    },
    RedundantRule {
        explanation: "an offset of `x = 0 y = 0` does not move the focus",
        key: "offset",
        kinds: &[BlockKind::Focus],
        repeatable: true,
        value: Redundant::Block(&[("x", "0"), ("y", "0")]),
    },
    RedundantRule {
        explanation: "focuses have an AI weight of 1 by default",
        key: "ai_will_do",
        kinds: &[BlockKind::Focus],
        repeatable: false,
        value: Redundant::Block(&[("factor", "1")]),
    },
    RedundantRule {
        explanation: "focuses have an AI weight of 1 by default",
        key: "ai_will_do",
        kinds: &[BlockKind::Focus],
        repeatable: false,
        value: Redundant::Block(&[("base", "1")]),
    },
    RedundantRule {
        explanation: "an empty `ai_will_do` is 1, the focus default",
        key: "ai_will_do",
        kinds: &[BlockKind::Focus],
        repeatable: false,
        value: Redundant::EmptyBlock,
    },
    RedundantRule {
        explanation: "an option's `ai_chance` is 1 when omitted",
        key: "ai_chance",
        kinds: &[BlockKind::EventOption],
        repeatable: false,
        value: Redundant::Block(&[("base", "1")]),
    },
    RedundantRule {
        explanation: "an option's `ai_chance` is 1 when omitted",
        key: "ai_chance",
        kinds: &[BlockKind::EventOption],
        repeatable: false,
        value: Redundant::Block(&[("factor", "1")]),
    },
    RedundantRule {
        explanation: "an empty `ai_chance` is 1, the same as omitting it",
        key: "ai_chance",
        kinds: &[BlockKind::EventOption],
        repeatable: false,
        value: Redundant::EmptyBlock,
    },
];

/// A kind of definition block, identified by where it sits in a file. See
/// [`block_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    /// `characters > <character> > advisor`.
    Advisor,
    /// `characters > <character>` and `characters > <character> > instance`.
    Character,
    /// `characters > <character> > corps_commander`.
    CorpsCommander,
    /// `characters > <character> > country_leader`.
    CountryLeader,
    /// `<category> > <decision>` in `common/decisions/*.txt` (missions too).
    Decision,
    /// `<category>` in `common/decisions/categories/*.txt`.
    DecisionCategory,
    /// A top-level event (`country_event`, `news_event`, ...).
    Event,
    /// `<event> > desc` / `<event> > title` written as `{ text = .. trigger = {..} }`.
    EventDesc,
    /// `<event> > option`.
    EventOption,
    /// `characters > <character> > field_marshal`.
    FieldMarshal,
    /// `focus_tree > focus`, and top-level `shared_focus` / `joint_focus`.
    Focus,
    /// `<focus> > offset`.
    FocusOffset,
    /// Top-level `focus_tree` (its own fields; `focus` children are sorted
    /// separately and deliberately unlisted).
    FocusTree,
    /// `ideas > <category> > <idea>`.
    Idea,
    /// `characters > <character> > navy_leader`.
    NavyLeader,
    /// `characters > <character> > scientist`.
    Scientist,
    /// `technologies > <technology>`.
    Technology,
    /// `technologies > <technology> > folder`.
    TechnologyFolder,
    /// `technologies > <technology> > path`.
    TechnologyPath,
}

impl BlockKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 19] = [
        Self::Advisor,
        Self::Character,
        Self::CorpsCommander,
        Self::CountryLeader,
        Self::Decision,
        Self::DecisionCategory,
        Self::Event,
        Self::EventDesc,
        Self::EventOption,
        Self::FieldMarshal,
        Self::Focus,
        Self::FocusOffset,
        Self::FocusTree,
        Self::Idea,
        Self::NavyLeader,
        Self::Scientist,
        Self::Technology,
        Self::TechnologyFolder,
        Self::TechnologyPath,
    ];

    /// Canonical order of this kind's top-level fields. Fields not listed stay
    /// where they are; repeated fields keep their relative order.
    pub const fn field_order(self) -> &'static [&'static str] {
        match self {
            Self::Advisor => &[
                "slot",
                "idea_token",
                "ledger",
                "name",
                "allowed",
                "visible",
                "available",
                "traits",
                "cost",
                "do_effect",
                "on_add",
                "on_remove",
                "can_be_fired",
                "ai_will_do",
            ],
            Self::Character => &[
                "allowed",
                "available",
                "name",
                "portraits",
                "gender",
                "allowed_civil_war",
                "instance",
                "country_leader",
                "advisor",
                "field_marshal",
                "corps_commander",
                "navy_leader",
                "scientist",
            ],
            Self::CorpsCommander | Self::FieldMarshal => &[
                "traits",
                "skill",
                "attack_skill",
                "defense_skill",
                "planning_skill",
                "logistics_skill",
                "legacy_id",
                "visible",
            ],
            Self::CountryLeader => &["desc", "ideology", "traits", "expire", "id"],
            Self::Decision => &[
                "name",
                "desc",
                "priority",
                "icon",
                "cosmetic_tag",
                "cosmetic_ideology",
                "allowed",
                "state_target",
                "targets",
                "targets_dynamic",
                "target_array",
                "target_root_trigger",
                "target_trigger",
                "visible",
                "available",
                "remove_trigger",
                "highlight_states",
                "on_map_mode",
                "ai_hint_pp_cost",
                "custom_cost_trigger",
                "custom_cost_text",
                "cost",
                "days_mission_timeout",
                "fire_only_once",
                "activation",
                "days_remove",
                "cancel_if_not_visible",
                "cancel_trigger",
                "days_re_enable",
                "fixed_random_seed",
                "is_good",
                "selectable_mission",
                "modifier",
                "targeted_modifier",
                "ai_will_do",
                "complete_effect",
                "remove_effect",
                "cancel_effect",
                "timeout_effect",
            ],
            Self::DecisionCategory => &[
                "icon",
                "picture",
                "priority",
                "scripted_gui",
                "allowed",
                "custom_icon",
                "visible_when_empty",
                "visible",
                "available",
                "visibility_type",
                "on_map_area",
            ],
            Self::Event => &[
                "id",
                "title",
                "desc",
                "picture",
                "major",
                "is_triggered_only",
                "hidden",
                "fire_only_once",
                "fire_for_sender",
                "trigger",
                "mean_time_to_happen",
                "immediate",
                "option",
            ],
            Self::EventDesc => &["text", "trigger"],
            // Effects inside an option are order-sensitive and unlisted, so
            // only these three permute among their own slots.
            Self::EventOption => &["name", "trigger", "ai_chance"],
            Self::Focus => &[
                "id",
                "allowed_is_joint",
                "icon",
                "overlay",
                "dynamic",
                "prerequisite",
                "mutually_exclusive",
                "x",
                "y",
                "relative_position_id",
                "cost",
                "text_icon",
                "will_lead_to_war_with",
                "allow_branch",
                "offset",
                "ai_will_do",
                "available",
                "bypass",
                "cancelable",
                "cancel_if_invalid",
                "continue_if_invalid",
                "joint_trigger",
                "available_if_capitulated",
                "search_filters",
                "select_effect",
                "complete_tooltip",
                "completion_reward",
                "completion_reward_joint_originator",
                "completion_reward_joint_member",
            ],
            Self::FocusOffset => &["x", "y", "trigger"],
            Self::FocusTree => &[
                "id",
                "country",
                "default",
                "reset_on_civilwar",
                "continuous_focus_position",
                "initial_show_position",
                "shortcut",
                "inlay_window",
                "alternate_icon_set",
                "shared_focus",
            ],
            Self::Idea => &[
                "name",
                "ledger",
                "picture",
                "allowed",
                "allowed_civil_war",
                "visible",
                "available",
                "cancel",
                "cost",
                "removal_cost",
                "level",
                "allowed_to_remove",
                "on_add",
                "on_remove",
                "research_bonus",
                "traits",
                "do_effect",
                "rule",
                "modifier",
                "equipment_bonus",
                "targeted_modifier",
                "cancel_if_invalid",
                "ai_will_do",
            ],
            Self::NavyLeader => &[
                "traits",
                "skill",
                "attack_skill",
                "defense_skill",
                "maneuvering_skill",
                "coordination_skill",
                "legacy_id",
                "visible",
            ],
            Self::Scientist => &["traits", "skills", "visible"],
            // Inline modifiers and unit-stat blocks are unlisted and stay put.
            Self::Technology => &[
                "doctrine_name",
                "allow_branch",
                "allow",
                "force_use_small_tech_layout",
                "is_special_project_tech",
                "enable_equipments",
                "show_equipment_icon",
                "enable_equipment_modules",
                "enable_subunits",
                "enable_building",
                "on_research_complete_limit",
                "on_research_complete",
                "show_effect_as_desc",
                "xp_research_type",
                "xp_unlock_cost",
                "xp_boost_cost",
                "xp_research_bonus",
                "enable_tactic",
                "path",
                "dependencies",
                "xor",
                "XOR",
                "doctrine",
                "research_cost",
                "start_year",
                "folder",
                "sub_technologies",
                "sub_tech_index",
                "categories",
                "special_project_specialization",
                "ai_will_do",
                "ai_research_weights",
            ],
            Self::TechnologyFolder => &["name", "position"],
            Self::TechnologyPath => &["leads_to_tech", "research_cost_coeff", "ignore_for_layout"],
        }
    }
}

/// Which kind of script file a path is, which decides the definition blocks
/// it can hold. See [`file_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// `common/characters/*.txt`.
    Characters,
    /// `common/decisions/categories/*.txt`.
    DecisionCategories,
    /// `common/decisions/*.txt`.
    Decisions,
    /// `events/*.txt`.
    Events,
    /// `common/ideas/*.txt`.
    Ideas,
    /// `common/national_focus/*.txt`.
    NationalFocus,
    /// Any other `*.txt` under `common/`, `events/` or `history/`.
    Other,
    /// `common/technologies/*.txt`.
    Technologies,
}

/// A value that has no effect for a given field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Redundant {
    /// A block holding exactly these `key = value` pairs, in any order.
    /// Values compare numerically when both sides are numbers, otherwise
    /// ASCII case-insensitively.
    Block(&'static [(&'static str, &'static str)]),
    /// A block with no entries.
    EmptyBlock,
    /// A scalar equal to the name of the block it is in (e.g. an idea's
    /// `picture`), provided that block has no `name` field.
    OwnName,
    /// A scalar, compared ASCII case-insensitively (numerically when both
    /// sides are numbers).
    Scalar(&'static str),
}

/// A field whose value has no effect in some kinds of block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RedundantRule {
    /// Why the value has no effect; suitable for a lint message.
    pub explanation: &'static str,
    /// The field key (exact case).
    pub key: &'static str,
    /// Kinds of block the rule applies to.
    pub kinds: &'static [BlockKind],
    /// Whether the key may legitimately repeat in a block (each occurrence
    /// then stands alone, e.g. `mutually_exclusive`). Non-repeatable keys are
    /// only reported when they occur exactly once in the block, since removing
    /// one of several occurrences could change which one the game uses.
    pub repeatable: bool,
    /// The redundant value.
    pub value: Redundant,
}

/// Classifies the block-valued entry at `path` (keys from the root down to and
/// including the entry's own key, as passed by [`crate::cst::visit_blocks`]).
pub fn block_kind(file: FileKind, path: &[&str]) -> Option<BlockKind> {
    match (file, path) {
        (FileKind::Characters, ["characters", _] | ["characters", _, "instance"]) => {
            Some(BlockKind::Character)
        }
        (FileKind::Characters, ["characters", _, role] | ["characters", _, "instance", role]) => {
            role_kind(role)
        }
        (FileKind::DecisionCategories, [_]) => Some(BlockKind::DecisionCategory),
        (FileKind::Decisions, [_, _]) => Some(BlockKind::Decision),
        (FileKind::Events, [event]) if EVENT_TYPES.contains(event) => Some(BlockKind::Event),
        (FileKind::Events, [event, "option"]) if EVENT_TYPES.contains(event) => {
            Some(BlockKind::EventOption)
        }
        (FileKind::Events, [event, "desc" | "title"]) if EVENT_TYPES.contains(event) => {
            Some(BlockKind::EventDesc)
        }
        (FileKind::Ideas, ["ideas", _, _]) => Some(BlockKind::Idea),
        (FileKind::NationalFocus, ["focus_tree"]) => Some(BlockKind::FocusTree),
        (FileKind::NationalFocus, ["focus_tree", "focus"]) => Some(BlockKind::Focus),
        (FileKind::NationalFocus, [root]) if ROOT_FOCUS_KEYS.contains(root) => {
            Some(BlockKind::Focus)
        }
        (FileKind::NationalFocus, ["focus_tree", "focus", "offset"]) => {
            Some(BlockKind::FocusOffset)
        }
        (FileKind::NationalFocus, [root, "offset"]) if ROOT_FOCUS_KEYS.contains(root) => {
            Some(BlockKind::FocusOffset)
        }
        (FileKind::Technologies, ["technologies", tech]) if !tech.starts_with('@') => {
            Some(BlockKind::Technology)
        }
        (FileKind::Technologies, ["technologies", tech, "folder"]) if !tech.starts_with('@') => {
            Some(BlockKind::TechnologyFolder)
        }
        (FileKind::Technologies, ["technologies", tech, "path"]) if !tech.starts_with('@') => {
            Some(BlockKind::TechnologyPath)
        }
        _ => None,
    }
}

/// Classifies a mod-relative path. Returns `None` for files the formatter and
/// the redundant-field lint do not touch: anything that is not a `*.txt` file
/// under `common/`, `events/` or `history/`.
pub fn file_kind(relative: &Path) -> Option<FileKind> {
    let is_txt = relative
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"));
    if !is_txt {
        return None;
    }
    let dirs: Vec<&str> = relative
        .parent()?
        .components()
        .map(|component| match component {
            Component::Normal(name) => name.to_str(),
            Component::CurDir
            | Component::ParentDir
            | Component::Prefix(_)
            | Component::RootDir => None,
        })
        .collect::<Option<_>>()?;
    match dirs.as_slice() {
        ["common", "characters"] => Some(FileKind::Characters),
        ["common", "decisions", "categories"] => Some(FileKind::DecisionCategories),
        ["common", "decisions"] => Some(FileKind::Decisions),
        ["common", "ideas"] => Some(FileKind::Ideas),
        ["common", "national_focus"] => Some(FileKind::NationalFocus),
        ["common", "technologies"] => Some(FileKind::Technologies),
        ["events"] => Some(FileKind::Events),
        ["common" | "events" | "history", ..] => Some(FileKind::Other),
        _ => None,
    }
}

/// The redundant-field rules that apply to `kind`, in [`REDUNDANT_RULES`]
/// order.
pub fn redundant_rules(kind: BlockKind) -> &'static [&'static RedundantRule] {
    // Looked up once per definition block, so the rules of each kind are
    // gathered once per run rather than filtered every time.
    static BY_KIND: LazyLock<HashMap<BlockKind, Vec<&'static RedundantRule>>> =
        LazyLock::new(|| {
            let mut by_kind: HashMap<BlockKind, Vec<&'static RedundantRule>> = HashMap::new();
            for rule in REDUNDANT_RULES {
                for &kind in rule.kinds {
                    by_kind.entry(kind).or_default().push(rule);
                }
            }
            by_kind
        });
    BY_KIND.get(&kind).map_or(&[], Vec::as_slice)
}

fn role_kind(role: &str) -> Option<BlockKind> {
    match role {
        "advisor" => Some(BlockKind::Advisor),
        "corps_commander" => Some(BlockKind::CorpsCommander),
        "country_leader" => Some(BlockKind::CountryLeader),
        "field_marshal" => Some(BlockKind::FieldMarshal),
        "navy_leader" => Some(BlockKind::NavyLeader),
        "scientist" => Some(BlockKind::Scientist),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_files() {
        let kind = |p: &str| file_kind(Path::new(p));
        assert_eq!(kind("common/decisions/MLT.txt"), Some(FileKind::Decisions));
        assert_eq!(
            kind("common/decisions/categories/MLT.txt"),
            Some(FileKind::DecisionCategories)
        );
        assert_eq!(kind("events/germany.txt"), Some(FileKind::Events));
        assert_eq!(kind("events/sub/germany.txt"), Some(FileKind::Other));
        assert_eq!(kind("history/countries/GER.txt"), Some(FileKind::Other));
        assert_eq!(
            kind("common/national_focus/x.TXT"),
            Some(FileKind::NationalFocus)
        );
        assert_eq!(kind("localisation/english/x.yml"), None);
        assert_eq!(kind("interface/x.txt"), None);
        assert_eq!(kind("descriptor.mod"), None);
        assert_eq!(kind("x.txt"), None);
    }

    #[test]
    fn classifies_blocks() {
        use BlockKind as B;
        use FileKind as F;
        assert_eq!(block_kind(F::Decisions, &["cat", "dec"]), Some(B::Decision));
        assert_eq!(block_kind(F::Decisions, &["cat"]), None);
        assert_eq!(block_kind(F::Decisions, &["cat", "dec", "available"]), None);
        assert_eq!(block_kind(F::Events, &["country_event"]), Some(B::Event));
        assert_eq!(
            block_kind(F::Events, &["news_event", "option"]),
            Some(B::EventOption)
        );
        assert_eq!(
            block_kind(F::Events, &["country_event", "desc"]),
            Some(B::EventDesc)
        );
        assert_eq!(block_kind(F::Events, &["add_namespace"]), None);
        assert_eq!(
            block_kind(F::NationalFocus, &["focus_tree", "focus"]),
            Some(B::Focus)
        );
        assert_eq!(
            block_kind(F::NationalFocus, &["shared_focus"]),
            Some(B::Focus)
        );
        assert_eq!(
            block_kind(F::NationalFocus, &["shared_focus", "offset"]),
            Some(B::FocusOffset)
        );
        assert_eq!(
            block_kind(F::Ideas, &["ideas", "country", "x"]),
            Some(B::Idea)
        );
        assert_eq!(
            block_kind(F::Characters, &["characters", "c", "instance", "advisor"]),
            Some(B::Advisor)
        );
        assert_eq!(block_kind(F::Technologies, &["technologies", "@x"]), None);
        assert_eq!(block_kind(F::Other, &["focus_tree", "focus"]), None);
    }

    #[test]
    fn user_example_order_holds() {
        let order = BlockKind::Decision.field_order();
        let pos = |k: &str| order.iter().position(|f| *f == k);
        assert!(pos("icon") < pos("complete_effect"));
    }

    #[test]
    fn field_orders_have_no_duplicates() {
        use std::collections::HashSet;
        for kind in BlockKind::ALL {
            let order = kind.field_order();
            let unique: HashSet<_> = order.iter().collect();
            assert_eq!(unique.len(), order.len(), "{kind:?}");
        }
    }

    #[test]
    fn rules_lookup() {
        let keys: Vec<_> = redundant_rules(BlockKind::Decision)
            .iter()
            .map(|r| r.key)
            .collect();
        assert!(keys.contains(&"fire_only_once"));
        assert!(!keys.contains(&"cancel_if_invalid"));
    }
}
