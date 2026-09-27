//! Builds the data of the rules site in `docs/` (`docs/rules.js`) and checks
//! that the committed copy is up to date, as clippy's `update_lints` does for
//! its lint list. After changing a rule, an option, or what hearty does to
//! one of the examples here, regenerate it with
//!
//! ```text
//! HEARTY_BLESS=1 cargo test --test docs
//! ```
//!
//! and commit `docs/rules.js`. Nothing in it is copied by hand:
//! - the redundant-field rules, the block kinds with their field orders, the
//!   options and the environment variables come from hearty itself
//!   (`hearty --rules-json`, see `src/docs.rs`);
//! - the prose about the other rules is here, and
//!   [`every_lint_and_change_is_documented`] checks that each lint
//!   diagnostic and each kind of formatting change in `src/` has a rule
//!   describing it;
//! - every example's "after" and output is what hearty makes of its
//!   "before", run on a mod of its own, in colour (`CLICOLOR_FORCE`), with
//!   the ANSI escapes turned into HTML. Each example also checks that hearty
//!   does what it shows: a redundant-field example must be reported by
//!   `--lint` and removed by `--fix`, a formatting example must make exactly
//!   the changes it is about, and what it formats must be formatted already.
//!
//! As in the integration tests, no run of hearty here may reach the
//! network: each lints with a fresh version cache and no steamcmd on `PATH`,
//! and sends its HTTP requests to a proxy on a closed port.

use rayon::prelude::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const BINARY: &str = env!("CARGO_BIN_EXE_hearty");

/// Set to `1` to write the site's data rather than check it.
const BLESS_VAR: &str = "HEARTY_BLESS";

/// Branches of the version cache every example lints with: the newest
/// version is 1.17.3.
const CACHED_BRANCHES: &[&str] = &["1.16.1", "1.17.3.0", "public"];

/// The site's data, generated here.
const DATA_FILE: &str = "docs/rules.js";

/// A proxy URL on a port nothing listens on.
const DEAD_PROXY: &str = "http://127.0.0.1:9";

/// The `descriptor.mod` of an example that brings none: up to date with
/// [`CACHED_BRANCHES`].
const DESCRIPTOR: &str = "name=\"Example Mod\"\nsupported_version=\"1.17.*\"\n";

/// Files outside the site whose links into it (to [`SITE_URL`]) must lead
/// somewhere.
const LINKING_FILES: &[&str] = &["README.md", "docs/development.md"];

/// Where the examples' localisation goes.
const LOCALISATION: &str = "localisation/english/example_l_english.yml";

/// The site's page, whose links must all lead somewhere.
const PAGE_FILE: &str = "docs/index.html";

/// The kinds of block a redundant-field rule's example is written in, most
/// familiar first: the first that the rule applies to is used.
const PREFERRED_KINDS: &[&str] = &[
    "focus",
    "decision",
    "event",
    "event_option",
    "idea",
    "focus_tree",
    "decision_category",
    "character",
    "advisor",
    "country_leader",
    "corps_commander",
    "field_marshal",
    "navy_leader",
    "scientist",
    "technology",
    "technology_folder",
    "technology_path",
    "event_desc",
    "focus_offset",
];

/// The script making the rest of the page's anchors and links.
const SCRIPT_FILE: &str = "docs/app.js";

/// Where the site is published (see `.github/workflows/pages.yml`).
const SITE_URL: &str = "https://jonathanwoollett-light.github.io/hearty/";

/// The mod the overview lints, in `my_mod` beside a fake Hearts of Iron IV.
const OVERVIEW: &[Source] = &[
    Source {
        path: "descriptor.mod",
        shown: true,
        text: "name=\"Example Mod\"\nsupported_version=\"1.16.*\"\n",
    },
    Source {
        path: "events/example.txt",
        shown: true,
        text: "add_namespace = example

country_event = {
	id = example.1
	title = example.1.t
	desc = example.1.d
	fire_only_once = no

	option = {
		name = example.1.a
		add_political_power = 50
	}
}
",
    },
    Source {
        path: LOCALISATION,
        shown: true,
        text: "l_english:
 example.1.t:0 \"The Example Crisis\"
 example.1.d:0 \"Something has happened.\"
",
    },
];

/// The groups of rules, in page order.
const GROUPS: &[Group] = &[
    Group {
        category: "lint",
        id: "localisation",
        intro: "",
        name: "Localisation",
    },
    Group {
        category: "lint",
        id: "descriptor",
        intro: "Checks of the mod's `descriptor.mod`. They are warnings: unlike missing \
                localisation and redundant fields, they don't make hearty exit with an error. \
                They run on a thread of their own, alongside the rest of the lint.",
        name: "descriptor.mod",
    },
    Group {
        category: "lint",
        id: "redundant",
        intro: "Fields set to their default value, or that otherwise have no effect. Each rule \
                below is one field and value, in the kinds of block listed: the definition \
                block's own fields, not those of a block nested in it (a decision's \
                `fire_only_once`, not one inside its `complete_effect`).

- Values compare as the game reads them: ASCII case-insensitively (`No` is `no`), quotes \
aside (`\"no\"` is `no`), and by value when both are numbers (`0.0` is `0`, but `5e1` is not \
`50`). Keys are case-sensitive, and only plain `=` assignments count.
- A block value must hold exactly the entries shown, in any order.
- A field that may only appear once is reported only where it does: removing one of \
several could change which one the game uses.
- [`--fix`](#--fix) removes each field with its line, tidying the blank lines around it. \
It never deletes a comment: a field with a comment inside it, or after it on its line, is \
reported but left in place, and comment lines above it stay.",
        name: "Redundant fields",
    },
    Group {
        category: "format",
        id: "sorting",
        intro: "",
        name: "Sorting",
    },
    Group {
        category: "format",
        id: "field_order",
        intro: "",
        name: "Field order",
    },
    Group {
        category: "format",
        id: "blocks",
        intro: "",
        name: "Blocks",
    },
    Group {
        category: "format",
        id: "spacing",
        intro: "",
        name: "Spacing",
    },
    Group {
        category: "format",
        id: "comments",
        intro: "",
        name: "Comments",
    },
];

/// The rules described here, in page order; the redundant-field rules,
/// described by the code, go after the lints'.
const RULES: &[Rule] = &[
    Rule {
        code: &["MissingLocalisation"],
        examples: &[Example {
            args: &["--lint"],
            changes: None,
            diagnostics_only: true,
            expect: &["\"example.1.a\" not localised in: english."],
            files: &[
                Source {
                    path: "events/example.txt",
                    shown: true,
                    text: "add_namespace = example

country_event = {
	id = example.1
	title = example.1.t
	desc = example.1.d
	is_triggered_only = yes

	option = {
		name = example.1.a
		add_political_power = 50
	}
}
",
                },
                Source {
                    path: LOCALISATION,
                    shown: true,
                    text: "l_english:
 example.1.t:0 \"The Example Crisis\"
 example.1.d:0 \"Something has happened.\"
",
                },
            ],
            title: "",
        }],
        fix: Some(false),
        group: "localisation",
        id: "missing_localisation",
        notes: "A value in square brackets (`title = \"[GetTitle]\"`) is scripted localisation, \
                which the game evaluates in place of a key, so it isn't reported. Each missing \
                key is reported once, in the first file (in path order) that uses it, where \
                that file defines it. The first {{max_shown}} are shown in full and the rest \
                counted.",
        summary: "A localisation key that an event, focus or technology uses but no \
                  localisation file defines.",
        title: "Missing localisation",
        what: "Reports each localisation key the mod's scripts use that no localisation file \
               of a checked language defines:

- an event's `title`, `desc` and option `name`s, in `events/*.txt`;
- a focus's `id`, in `common/national_focus/*.txt`;
- every technology, in `common/technologies/*.txt`.

Localisation is read from the files anywhere under `localisation/` (`replace/` included) whose \
names end with `l_<language>.yml`, for the languages checked: English, unless \
[`--lang`](#--lang) or [`--all`](#--all) say otherwise.

The base game's localisation counts too, since most mods reuse vanilla's events, focuses and \
ideas, except in the folders the mod's `descriptor.mod` replaces with `replace_path`. hearty \
finds Hearts of Iron IV in your Steam libraries, or takes its folder from \
[`--game-dir`](#--game-dir) or [`HEARTY_GAME_DIR`](#HEARTY_GAME_DIR). Where the game isn't \
installed, as in CI, keys only the base game defines are reported too, and the output says so.",
        why: "The game shows a missing key as the key itself (`example.1.a`), in every language \
              that lacks it.",
    },
    Rule {
        code: &["DuplicateDescriptorKey"],
        examples: &[Example {
            args: &["--lint"],
            changes: None,
            diagnostics_only: true,
            expect: &["duplicate key \"name\" in descriptor.mod."],
            files: &[Source {
                path: "descriptor.mod",
                shown: true,
                text: "version=\"1.0\"
name=\"Example Mod\"
tags={
	\"Gameplay\"
}
name=\"Example Mod (beta)\"
supported_version=\"1.17.*\"
",
            }],
            title: "",
        }],
        fix: Some(false),
        group: "descriptor",
        id: "descriptor_duplicate_key",
        notes: "`replace_path` may repeat, and is never reported.",
        summary: "A key that `descriptor.mod` gives more than once.",
        title: "Duplicate key in descriptor.mod",
        what: "Reports a key that `descriptor.mod` gives twice or more, such as two `name`s.",
        why: "Only one of the values can take effect, so the others are at best dead weight, and \
              which one wins is easy to get wrong.",
    },
    Rule {
        code: &["DescriptorVersionMismatch"],
        examples: &[Example {
            args: &["--lint"],
            changes: None,
            diagnostics_only: true,
            expect: &[
                "descriptor.mod supported_version \"1.16.*\" does not match latest HOI4 1.17.3.",
            ],
            files: &[Source {
                path: "descriptor.mod",
                shown: true,
                text: "name=\"Example Mod\"\nsupported_version=\"1.16.*\"\n",
            }],
            title: "",
        }],
        fix: Some(false),
        group: "descriptor",
        id: "descriptor_supported_version",
        notes: "Nothing is reported when no version can be found, say without a network. In CI, \
                cache the version with `actions/cache` (see \
                [`HEARTY_CACHE_DIR`](#HEARTY_CACHE_DIR)) so steamcmd only runs when the cache \
                is stale.",
        summary: "A `supported_version` in `descriptor.mod` that doesn't match the latest HOI4 \
                  release.",
        title: "Outdated supported_version",
        what: "Checks the version pattern of `descriptor.mod`'s `supported_version` (such as \
               `1.17.*`) against the latest release of Hearts of Iron IV, taken from the game's \
               Steam app info. hearty caches that for a day (see \
               [`HEARTY_CACHE_DIR`](#HEARTY_CACHE_DIR)), and otherwise asks steamcmd for it, \
               downloading steamcmd if it can't find it.",
        why: "The launcher warns players that a mod made for another version of the game may not \
              work, which puts them off a mod that works fine; an up-to-date \
              `supported_version` also tells them it's maintained.",
    },
    Rule {
        code: &["FocusesReordered"],
        examples: &[Example {
            args: &["--format"],
            changes: Some(&["3 focuses moved"]),
            diagnostics_only: false,
            expect: &[],
            files: &[Source {
                path: "common/national_focus/example.txt",
                shown: true,
                text: "focus_tree = {
	id = EXA_focus_tree

	# Needs the national plan, so it goes after it.
	focus = {
		id = EXA_army_reform
		prerequisite = { focus = EXA_national_plan }
		x = -1
		y = 1
		relative_position_id = EXA_national_plan
		cost = 10
	}

	focus = {
		id = EXA_naval_reform
		prerequisite = { focus = EXA_national_plan }
		x = 1
		y = 1
		relative_position_id = EXA_national_plan
		cost = 10
	}

	focus = {
		id = EXA_national_plan
		x = 5
		y = 0
		cost = 10
	}
}
",
            }],
            title: "",
        }],
        fix: None,
        group: "sorting",
        id: "sort_focuses",
        notes: "A focus tree is left as it is if a focus that would move doesn't start its own \
                line (it follows the tree's `{` or another block on its line), and a whole file \
                if it doesn't parse, if two focuses of a tree share an `id`, or if the focuses \
                depend on each other in a cycle.",
        summary: "Sorts each focus tree so every focus comes after the focuses it depends on.",
        title: "Focus sorting",
        what: "Sorts the `focus` blocks of each top-level `focus_tree` in \
               `common/national_focus/*.txt` so that each comes after the focus it's positioned \
               relative to (`relative_position_id`) and after its `prerequisite`s. Otherwise \
               focuses keep their order: the sort is stable, so sorting sorted focuses changes \
               nothing.

A focus moves as a whole: its lines, the comment lines right above it and a comment after its \
closing brace. Anything else after its closing brace on its line (another entry, or the `}` \
closing the tree) stays where it is, on a line of its own. Between two focuses that move, a gap \
of blank lines becomes exactly one (see [blank lines](#blank_line_separators)).",
        why: "The tree then reads in the order its focuses are taken, and each focus's position \
              refers to a focus already read.",
    },
    Rule {
        code: &["EventsReordered"],
        examples: &[Example {
            args: &["--format"],
            changes: Some(&["2 events moved"]),
            diagnostics_only: false,
            expect: &[],
            files: &[Source {
                path: "events/example.txt",
                shown: true,
                text: "add_namespace = example

country_event = {
	id = example.2
	title = example.2.t
	is_triggered_only = yes
	option = { name = example.2.a }
}

country_event = {
	id = example.10
	title = example.10.t
	is_triggered_only = yes
	option = { name = example.10.a }
}

country_event = {
	id = example.1
	title = example.1.t
	is_triggered_only = yes
	option = {
		name = example.1.a
		country_event = example.10
	}
}
",
            }],
            title: "",
        }],
        fix: None,
        group: "sorting",
        id: "sort_events",
        notes: "An event file is left as it is if an event that would move doesn't start its own \
                line, if it doesn't parse, if two of its events share an `id`, or if its events \
                fire each other in a cycle.",
        summary: "Sorts an event file's events so each chain of events stays together, in \
                  natural order of their ids.",
        title: "Event sorting",
        what: "Sorts the top-level events (`country_event`, `news_event`, ...) of each \
               `events/*.txt` file into groups of events connected by the events they fire \
               (`country_event = example.10`, or `country_event = { id = example.10 }`, \
               anywhere in an event). Groups come in natural order of their earliest event id \
               (`example.2` before `example.10`), and so do the events of a group, each after \
               the events that fire it. The sort is stable, so sorting sorted events changes \
               nothing.

An event moves as a whole, like a focus (see [focus sorting](#sort_focuses)), and between two \
events that move a gap of blank lines becomes exactly one.",
        why: "A chain of events reads top to bottom in the order it fires, and an event is found \
              where its id says it is.",
    },
    Rule {
        code: &["SeparatorsNormalised"],
        examples: &[Example {
            args: &["--format"],
            changes: Some(&["1 blank-line fix"]),
            diagnostics_only: false,
            expect: &[],
            files: &[Source {
                path: "events/example.txt",
                shown: true,
                text: "add_namespace = example

country_event = {
	id = example.1
	title = example.1.t
	option = { name = example.1.a }
}
country_event = {
	id = example.2
	title = example.2.t
	option = { name = example.2.a }
}



country_event = {
	id = example.3
	title = example.3.t
	option = { name = example.3.a }
}
",
            }],
            title: "",
        }],
        fix: None,
        group: "sorting",
        id: "blank_line_separators",
        notes: "Only blank lines change: a comment or another entry between two focuses or \
                events stays where it is. A file counts one blank-line fix when its separators \
                change but nothing moves; when focuses or events move, the moves are counted \
                instead.",
        summary: "Exactly one blank line between the focuses of a tree, and between the events of \
                  a file.",
        title: "Blank lines between focuses and events",
        what: "Wherever [focus sorting](#sort_focuses) and [event sorting](#sort_events) apply, \
               the gap of blank lines between two focuses of a tree, or two events of a file, \
               becomes exactly one blank line: none become one, and several become one. This \
               happens whether or not anything moves.",
        why: "Each focus and event stands apart the same way, as in vanilla, and sorting never \
              leaves blocks run together or far apart.",
    },
    Rule {
        code: &["FieldsReordered"],
        examples: &[Example {
            args: &["--format"],
            changes: Some(&["1 block with reordered fields"]),
            diagnostics_only: false,
            expect: &[],
            files: &[Source {
                path: "common/decisions/example.txt",
                shown: true,
                text: "EXA_decisions = {
	EXA_build_factory = {
		complete_effect = {
			add_offsite_building = { type = industrial_complex level = 1 }
		}
		# Only in peacetime.
		available = { has_war = no }
		cost = 50
		icon = generic_industry
		ai_will_do = { base = 5 }
	}
}
",
            }],
            title: "",
        }],
        fix: None,
        group: "field_order",
        id: "field_order",
        notes: "A block whose fields share a line with each other or with its braces is left as \
                it is; the definition blocks nested in it are still sorted.",
        summary: "Puts the fields of focuses, decisions, events, ideas, characters and \
                  technologies in vanilla's order.",
        title: "Field order",
        what: "Sorts the fields of each definition block into the order of its kind, listed \
               below. The orders were mined from vanilla HOI4 and three large mods, with \
               contested pairs settled by a reading order: identity, position, cost, \
               prerequisites, gates, modifiers, AI, effects.

Only the listed fields move, and only among the places listed fields already hold: fields hearty \
doesn't know and bare values keep their places, and repeated fields (several `option` or \
`prerequisite` fields) keep their order among themselves. A field moves as a whole, with the \
comment lines right above it and a comment after it on its line; blank lines, and comments set \
apart from fields by them, stay where they are.",
        why: "Every block of a kind reads the same way: what it is, where it sits, what it costs, \
              when it's allowed, what it does. A missing or out-of-place field stands out, and \
              diffs are about what changed.",
    },
    Rule {
        code: &["BlocksJoined"],
        examples: &[Example {
            args: &["--format"],
            changes: Some(&["3 blocks joined onto one line"]),
            diagnostics_only: false,
            expect: &[],
            files: &[Source {
                path: "common/national_focus/example.txt",
                shown: true,
                text: "focus_tree = {
	id = EXA_focus_tree

	focus = {
		id = EXA_army_reform
		prerequisite = {
			focus = EXA_national_plan
		}
		mutually_exclusive = {
			focus = EXA_naval_reform
		}
		x = 4
		y = 1
		cost = 10
		available = {
			has_war = no
			has_government = democratic
		}
		search_filters = {
			FOCUS_FILTER_ARMY_XP
			FOCUS_FILTER_MANPOWER
		}
		completion_reward = {
			# Pays for the reforms.
			add_political_power = 120
		}
	}
}
",
            }],
            title: "",
        }],
        fix: None,
        group: "blocks",
        id: "join_short_blocks",
        notes: "Blocks with more than one `key = value` entry, a nested block, a comment or a \
                multi-line string stay as they are, and so do empty blocks, which vanilla mostly \
                writes over two lines. Joining is one-way: a block already on one line is never \
                split, however long.",
        summary: "Joins a block holding one `key = value`, or a list of values, onto one line \
                  when it fits.",
        title: "Joining short blocks",
        what: "Joins a block written over several lines onto one line, as `key = { focus = \
               EXA_national_plan }`, when it holds exactly one `key = value` entry (or another \
               comparison, such as `has_political_power > 100`) or only bare values, has no \
               comment, isn't a definition block (a focus, decision, event, ...), and the line \
               it makes fits in [`--max-width`](#--max-width) columns, a tab counting as \
               {{tab_width}}.",
        why: "`prerequisite = { focus = EXA_national_plan }` says in one line what took three, \
              as vanilla writes it, so files are shorter and quicker to scan.",
    },
    Rule {
        code: &["SpacingFixed"],
        examples: &[Example {
            args: &["--format"],
            changes: Some(&["5 spacing fixes"]),
            diagnostics_only: false,
            expect: &[],
            files: &[Source {
                path: "common/decisions/example.txt",
                shown: true,
                text: "EXA_decisions = {
	EXA_build_factory = {
		icon=generic_industry
		available = {has_war = no}
		cost   =   50
		modifier = {  political_power_gain=0.1  }
		ai_will_do = {
			base = 5
			modifier = { factor=0 has_political_power<100 }
		}
	}
}
",
            }],
            title: "",
        }],
        fix: None,
        group: "spacing",
        id: "normalise_spacing",
        notes: "A gap holding a line break or a comment is left alone, and quoted strings are \
                copied as they are.",
        summary: "One space around operators, and inside the braces of one-line blocks.",
        title: "Spacing",
        what: "Puts exactly one space between a key and its operator, an operator and its value, \
               a key and the `{` of a block with no operator, and a tag and its `{` (`rgb { .. \
               }`), where the gap holds only spaces and tabs. Every block on one line is \
               rewritten in the same form: `{ }` when empty, else `{ `, its entries separated by \
               single spaces, and ` }`.",
        why: "`x=-1`, `x = -1` and `x =  -1` mean the same: writing it one way keeps files \
              consistent, and diffs about what changed.",
    },
    Rule {
        code: &["CommentsReflowed"],
        examples: &[
            Example {
                args: &["--format"],
                changes: Some(&["3 comments reflowed"]),
                diagnostics_only: false,
                expect: &[],
                files: &[Source {
                    path: "common/decisions/example.txt",
                    shown: true,
                    text: "# The example decisions. Each costs political power, and the AI weighs them by how much it has to spare, so a
# country at peace takes them sooner.
EXA_decisions = {
	EXA_build_factory = {
		# Builds a civilian factory in the capital. Not available at war, when the factories are needed for the front.
		icon = generic_industry
		available = { has_war = no }
		cost = 50
		# Notes:
		# - The cost goes up by 25 each time the decision is taken, so taking it early is cheaper than waiting for war.
		# - The AI only takes it with political power to spare.
		complete_effect = {
			add_offsite_building = { type = industrial_complex level = 1 }
		}
	}
}
",
                }],
                title: "Prose wider than `--max-width` is rewrapped",
            },
            Example {
                args: &["--format"],
                changes: Some(&[]),
                diagnostics_only: false,
                expect: &[],
                files: &[Source {
                    path: "common/decisions/example.txt",
                    shown: true,
                    text: "##################################################################################################################
# Commented-out script, decorations and tables are left as they are, however wide.
##################################################################################################################
EXA_decisions = {
	EXA_build_factory = {
		icon = generic_industry
		#complete_effect = { add_offsite_building = { type = industrial_complex level = 1 } add_political_power = -50 }
		# Level    Cost    Factories    When                                                                    Notes
		# 1        50      1            peace                                                                   cheap
		# Lines that fit are never joined,
		# even when they would fit on one line.
		cost = 50
	}
}
",
                }],
                title: "Everything else is left as it is",
            },
        ],
        fix: None,
        group: "comments",
        id: "reflow_comments",
        notes: "Paragraphs whose indentation and `#` marks leave fewer than {{min_text_columns}} \
                columns for text are left alone, as are comments after code on their line \
                (`x = y # why`). Running the rule on its own output changes nothing.",
        summary: "Rewraps comments of prose with a line wider than `--max-width`.",
        title: "Comment reflow",
        what: "Rewraps each paragraph of prose in full-line comments that has a line wider than \
               [`--max-width`](#--max-width) columns (a tab counting as {{tab_width}}). From the \
               paragraph's first line that is too wide to its end, the words are refilled \
               greedily, each line taking as many as fit after the paragraph's indentation and \
               `#` marks. Only the spaces and line breaks between words change: every word is kept, \
               in order, and new lines end like the lines they replace (CRLF stays CRLF).

A line is prose unless it is commented-out script or data (`#has_war = yes`, `# NOT = {`, a lone \
token such as `# GER_focus_x`, a line inside a commented-out block), a decoration (`# ----`, \
`### Focus tree ###`, or more than {{max_marker}} `#` marks), text lined up in columns (a tab, or two \
spaces in a row mid-sentence), or empty. Such lines are never changed, and end a paragraph.

A paragraph is a run of prose lines with the same indentation and `#` marks, their text starting in \
the same column, each carrying on the text of the line above. HOI4 comments are as often a stack \
of one-line notes as wrapped prose, so a line starts a note of its own when it begins with a \
label (`TODO:`, `Note:`) or the line above ends a sentence; and where that's in doubt (a line \
starting with a capital letter or a token of script after an unfinished sentence, a line whose \
first word would have fitted on the line above, or lines wider than {{unwrapped_width}} \
columns, or than [`--max-width`](#--max-width) plus {{unwrapped_margin}} if that is more, which \
nobody wraps by hand) the paragraphs are left as they are. List items (`- `, \
`* `, `+ `, `• `, or a number of up to {{max_item_digits}} digits and `. ` or `) `) start \
paragraphs of their own, their lines hanging under the item's text.

A paragraph with no line too wide is never touched: lines that fit are never joined, and a line \
wider only for holding a single word, such as a URL, is no reason to rewrap.",
        why: "Long comment lines run off the side of editors and diffs, and rewrapping them by hand \
              after every edit is tedious.",
    },
];

/// More about some options than `--help` says, by option id.
const OPTION_DETAILS: &[(&str, &str)] = &[
    (
        "check",
        "Runs the formatter without writing, prints what it would change, and exits with an \
         error if any file would change: use it in CI.",
    ),
    (
        "fix",
        "Removes [redundant fields](#group-redundant), except those whose removal would delete \
         a comment, and prints what it changed.",
    ),
    (
        "flamegraph",
        "Open the SVG in a browser: hover over a frame for its time, click to zoom. It works \
         with any actions, and doesn't change what they do.",
    ),
    (
        "format",
        "Applies every [formatting rule](#formatting) to the `*.txt` files under `common/`, \
         `events/` and `history/`, and prints what it changed.",
    ),
    (
        "lint",
        "Reports [missing localisation](#missing_localisation), \
         [`descriptor.mod` problems](#group-descriptor) and \
         [redundant fields](#group-redundant), and exits with an error if it finds missing \
         localisation or redundant fields: `descriptor.mod` problems are only warnings. It \
         needs a `descriptor.mod` in the mod's folder.",
    ),
    (
        "timings",
        "`active` is the time threads spent in a part and the parts under it, summed over \
         threads; `self` is the part of it not spent in a part nested in it; `share` is its \
         share of the total, parts under 0.5% being folded into a `… N more` line.",
    ),
];

/// Examples of some options, by option id.
const OPTION_EXAMPLES: &[(&str, &[Example])] = &[
    (
        "check",
        &[Example {
            args: &["--check"],
            changes: Some(&["2 spacing fixes"]),
            diagnostics_only: false,
            expect: &["formatting check failed"],
            files: &[Source {
                path: "common/decisions/example.txt",
                shown: true,
                text: "EXA_decisions = {
	EXA_build_factory = {
		icon=generic_industry
		cost   =   50
	}
}
",
            }],
            title: "",
        }],
    ),
    (
        "fix",
        &[Example {
            args: &["--fix"],
            changes: Some(&["1 redundant field removed"]),
            diagnostics_only: false,
            expect: &[],
            files: &[Source {
                path: "common/decisions/example.txt",
                shown: true,
                text: "EXA_decisions = {
	EXA_build_factory = {
		icon = generic_industry
		cost = 50
		fire_only_once = no
	}
}
",
            }],
            title: "",
        }],
    ),
    (
        "max_width",
        &[
            Example {
                args: &["--format"],
                changes: Some(&["1 block joined onto one line"]),
                diagnostics_only: false,
                expect: &[],
                files: MAX_WIDTH_EXAMPLE,
                title: "`--max-width 100` (the default)",
            },
            Example {
                args: &["--format", "--max-width", "60"],
                changes: Some(&["1 comment reflowed"]),
                diagnostics_only: false,
                expect: &[],
                files: MAX_WIDTH_EXAMPLE,
                title: "`--max-width 60`",
            },
        ],
    ),
];

/// What the `--max-width` examples format.
const MAX_WIDTH_EXAMPLE: &[Source] = &[Source {
    path: "common/decisions/example.txt",
    shown: true,
    text: "EXA_decisions = {
	EXA_build_factory = {
		# Builds a civilian factory in the capital, if there is room for one.
		icon = generic_industry
		available = {
			has_completed_focus = EXA_industrial_expansion_program
		}
		complete_effect = {
			add_offsite_building = { type = industrial_complex level = 1 }
		}
	}
}
",
}];

/// What the environment variables hearty reads do, by name.
const ENVIRONMENT: &[(&str, &str)] = &[
    (
        "CLICOLOR_FORCE",
        "Set to anything but `0` to print in colour even when the output isn't a terminal, such \
         as in a CI log.",
    ),
    (
        "HEARTY_CACHE_DIR",
        "Where the lint caches the latest HOI4 version, for a day, and keeps the steamcmd it \
         downloads. In GitHub Actions, cache it with `actions/cache` so steamcmd only runs when \
         the cache is stale.",
    ),
    (
        "HEARTY_GAME_DIR",
        "Hearts of Iron IV's folder, whose localisation counts as the mod's, when \
         [`--game-dir`](#--game-dir) isn't given. Set it to an empty value to leave the base \
         game out.",
    ),
    (
        "NO_COLOR",
        "Set to anything but an empty value to print without colour.",
    ),
];

/// A mod's file for an example to run hearty on.
#[derive(Debug, Clone, Copy)]
struct Source {
    /// Relative to the mod's folder, with `/`.
    path: &'static str,
    /// Whether the site shows the file (a localisation file that only keeps
    /// an example free of missing keys need not be).
    shown: bool,
    text: &'static str,
}

/// An example of a rule or an option: hearty run on a mod of its own.
#[derive(Debug)]
struct Example {
    /// The arguments after `hearty`, run in the mod's folder.
    args: &'static [&'static str],
    /// The changes the summary of `--format`, `--check` or `--fix` must
    /// list (its `Changes:` line), none meaning that nothing changed; `None`
    /// for a lint.
    changes: Option<&'static [&'static str]>,
    /// Whether to show just the diagnostics (stderr, but its closing error)
    /// rather than all the output.
    diagnostics_only: bool,
    /// Text the output must hold, ANSI escapes stripped.
    expect: &'static [&'static str],
    files: &'static [Source],
    /// A caption, in the site's Markdown; none if empty.
    title: &'static str,
}

/// A group of rules on the site.
#[derive(Debug)]
struct Group {
    /// `lint` or `format`.
    category: &'static str,
    id: &'static str,
    /// Prose above the group's rules, in the site's Markdown.
    intro: &'static str,
    name: &'static str,
}

/// A rule described here. Its prose is the site's Markdown (see `docs/app.js`:
/// paragraphs, `- ` lists, `code`, **bold** and `[links](#id)`), where
/// `{{name}}` stands for the constant `name` of `hearty --rules-json`.
#[derive(Debug)]
struct Rule {
    /// The diagnostics or kinds of change (`src/report.rs`'s `Change`) in the
    /// code that the rule describes.
    code: &'static [&'static str],
    examples: &'static [Example],
    /// For a lint, whether `--fix` fixes it.
    fix: Option<bool>,
    group: &'static str,
    id: &'static str,
    /// Limits and fine print.
    notes: &'static str,
    summary: &'static str,
    title: &'static str,
    what: &'static str,
    why: &'static str,
}

/// A template for the example of a redundant-field rule: a file holding a
/// block of one kind, whose fields are written in canonical order, with the
/// redundant field placed among them by the kind's field order.
#[derive(Debug)]
struct Template {
    /// The block's fields: key and text (whose lines after the first are
    /// indented in full).
    fields: &'static [(&'static str, &'static str)],
    /// The text before the fields.
    head: &'static str,
    /// The fields' indentation.
    indent: &'static str,
    /// The block kind's id.
    kind: &'static str,
    /// Localisation for the keys the file uses.
    localisation: &'static str,
    /// The block's own name (for a rule whose value is its name).
    owner: &'static str,
    path: &'static str,
    /// The text after the fields.
    tail: &'static str,
}

/// Templates for the redundant-field rules' examples; see [`PREFERRED_KINDS`].
const TEMPLATES: &[Template] = &[
    Template {
        fields: &[
            ("id", "id = EXA_national_plan"),
            ("icon", "icon = GFX_goal_generic_political_pressure"),
            ("x", "x = 5"),
            ("y", "y = 0"),
            ("cost", "cost = 10"),
            (
                "completion_reward",
                "completion_reward = { add_political_power = 120 }",
            ),
        ],
        head: "focus_tree = {\n\tid = EXA_focus_tree\n\n\tfocus = {\n",
        indent: "\t\t",
        kind: "focus",
        localisation: "l_english:\n EXA_national_plan:0 \"National Plan\"\n",
        owner: "",
        path: "common/national_focus/example.txt",
        tail: "\t}\n}\n",
    },
    Template {
        fields: &[
            ("id", "id = EXA_focus_tree"),
            (
                "focus",
                "focus = {\n\t\tid = EXA_national_plan\n\t\tx = 5\n\t\ty = 0\n\t\tcost = 10\n\t}",
            ),
        ],
        head: "focus_tree = {\n",
        indent: "\t",
        kind: "focus_tree",
        localisation: "l_english:\n EXA_national_plan:0 \"National Plan\"\n",
        owner: "",
        path: "common/national_focus/example.txt",
        tail: "}\n",
    },
    Template {
        fields: &[
            ("icon", "icon = generic_industry"),
            ("available", "available = { has_war = no }"),
            ("cost", "cost = 50"),
            (
                "complete_effect",
                "complete_effect = {\n\t\t\tadd_offsite_building = { type = industrial_complex level = 1 }\n\t\t}",
            ),
        ],
        head: "EXA_decisions = {\n\tEXA_build_factory = {\n",
        indent: "\t\t",
        kind: "decision",
        localisation: "",
        owner: "",
        path: "common/decisions/example.txt",
        tail: "\t}\n}\n",
    },
    Template {
        fields: &[
            ("icon", "icon = generic_industry"),
            ("picture", "picture = GFX_decision_cat_generic_industry"),
            ("allowed", "allowed = { tag = EXA }"),
        ],
        head: "EXA_decisions = {\n",
        indent: "\t",
        kind: "decision_category",
        localisation: "",
        owner: "",
        path: "common/decisions/categories/example.txt",
        tail: "}\n",
    },
    Template {
        fields: &[
            ("id", "id = example.1"),
            ("title", "title = example.1.t"),
            ("desc", "desc = example.1.d"),
            ("picture", "picture = GFX_report_event_generic_read_write"),
            ("is_triggered_only", "is_triggered_only = yes"),
            (
                "option",
                "option = {\n\t\tname = example.1.a\n\t\tadd_political_power = 50\n\t}",
            ),
        ],
        head: "add_namespace = example\n\ncountry_event = {\n",
        indent: "\t",
        kind: "event",
        localisation: EVENT_LOCALISATION,
        owner: "",
        path: "events/example.txt",
        tail: "}\n",
    },
    Template {
        fields: &[
            ("name", "name = example.1.a"),
            ("add_political_power", "add_political_power = 50"),
        ],
        head: "add_namespace = example\n\ncountry_event = {\n\tid = example.1\n\ttitle = example.1.t\n\tdesc = example.1.d\n\tis_triggered_only = yes\n\toption = {\n",
        indent: "\t\t",
        kind: "event_option",
        localisation: EVENT_LOCALISATION,
        owner: "",
        path: "events/example.txt",
        tail: "\t}\n}\n",
    },
    Template {
        fields: &[
            ("allowed", "allowed = { original_tag = EXA }"),
            ("modifier", "modifier = { consumer_goods_factor = -0.1 }"),
        ],
        head: "ideas = {\n\tcountry = {\n\t\tEXA_war_economy = {\n",
        indent: "\t\t\t",
        kind: "idea",
        localisation: "",
        owner: "EXA_war_economy",
        path: "common/ideas/example.txt",
        tail: "\t\t}\n\t}\n}\n",
    },
    Template {
        fields: &[
            ("name", "name = EXA_john_smith"),
            (
                "portraits",
                "portraits = {\n\t\t\tcivilian = { large = GFX_portrait_generic }\n\t\t}",
            ),
            (
                "country_leader",
                "country_leader = {\n\t\t\tideology = liberalism\n\t\t\texpire = \"1965.1.1.1\"\n\t\t}",
            ),
        ],
        head: "characters = {\n\tEXA_john_smith = {\n",
        indent: "\t\t",
        kind: "character",
        localisation: "",
        owner: "",
        path: "common/characters/example.txt",
        tail: "\t}\n}\n",
    },
];

/// Localisation for the keys of the event templates.
const EVENT_LOCALISATION: &str = "l_english:
 example.1.t:0 \"The Example Crisis\"
 example.1.d:0 \"Something has happened.\"
 example.1.a:0 \"Very well.\"
";

/// A mod to run hearty on, in a temporary folder.
struct Mod {
    /// Removed when the mod is dropped.
    _dir: tempfile::TempDir,
    /// The mod's folder.
    root: PathBuf,
}

impl Mod {
    /// A mod in `my_mod` in a temporary folder, holding `files` and, unless
    /// they have one, [`DESCRIPTOR`].
    fn new<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("my_mod");
        let mut descriptor = false;
        for (path, text) in files {
            assert!(
                !text.contains('\\') && !path.contains('\\'),
                "{path}: output paths are made portable by turning `\\` into `/`, so examples \
                 can't hold one"
            );
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            descriptor |= path.ends_with("descriptor.mod");
        }
        if !descriptor {
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("descriptor.mod"), DESCRIPTOR).unwrap();
        }
        Self { _dir: dir, root }
    }

    /// The text of the mod's file at `path`.
    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.join(path)).unwrap()
    }
}

/// What a run of hearty printed, with portable paths (see [`portable`]).
struct Run {
    /// stdout and stderr together, as a terminal shows them.
    output: String,
    success: bool,
}

/// A `hearty` command with `args`, run in `cwd`, linting with the version
/// cache in `cache` and no base game, in colour, and never reaching the
/// network.
fn hearty(cwd: &Path, args: &[&str], cache: &Path) -> Command {
    let mut command = Command::new(BINARY);
    command
        .current_dir(cwd)
        .args(args)
        .env("HEARTY_CACHE_DIR", cache)
        // No steamcmd to run: the cache is fresh anyway.
        .env("PATH", cache)
        .env("HEARTY_GAME_DIR", "")
        .env("CLICOLOR_FORCE", "1")
        .env_remove("NO_COLOR")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy");
    for var in ["ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY"] {
        command.env(var, DEAD_PROXY);
    }
    command
}

/// Runs `command`, capturing its stdout and stderr together, in the order
/// they were printed, as a terminal shows them.
fn run(mut command: Command) -> Run {
    let (mut reader, writer) = std::io::pipe().unwrap();
    command.stdout(writer.try_clone().unwrap()).stderr(writer);
    let mut child = command.spawn().unwrap();
    // The command holds the pipe's write ends: drop them, or reading never
    // ends.
    drop(command);
    let mut output = String::new();
    reader.read_to_string(&mut output).unwrap();
    Run {
        output: portable(&output),
        success: child.wait().unwrap().success(),
    }
}

/// Runs `command`, a lint (which writes nothing), returning its stdout, its
/// diagnostics (its stderr but the error closing the run, if it failed) and
/// that error (empty if it passed).
fn lint_run(mut command: Command) -> (String, String, String) {
    let output = command.output().unwrap();
    let stdout = portable(&String::from_utf8(output.stdout).unwrap());
    let stderr = portable(&String::from_utf8(output.stderr).unwrap());
    let mut lines: Vec<&str> = stderr.lines().collect();
    let mut closing = String::new();
    if !output.status.success() {
        closing = lines.pop().unwrap_or_default().to_owned();
        assert!(
            closing.starts_with("Error: \"lint found "),
            "{stdout}{stderr}"
        );
    }
    let diagnostics = lines.iter().map(|line| format!("{line}\n")).collect();
    (stdout, diagnostics, closing)
}

/// `output` as it reads on every platform: `/` in paths, the binary called
/// `hearty`, and without the line saying how long the run took.
fn portable(output: &str) -> String {
    output
        .replace('\\', "/")
        .replace("hearty.exe", "hearty")
        .lines()
        .filter(|line| !line.starts_with("Finished in "))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// `text` without ANSI escapes.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('\x1b') {
        out.push_str(&rest[..start]);
        let end = rest[start..].find('m').unwrap();
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

/// `text` escaped for HTML.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The style ANSI SGR escapes set: bold, dim, italic, underline and a
/// foreground colour of the 16 (0 to 15).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Style {
    bold: bool,
    dim: bool,
    fg: Option<u8>,
    italic: bool,
    underline: bool,
}

impl Style {
    /// The CSS classes of the style (see `docs/style.css`).
    fn classes(self) -> String {
        let mut classes = Vec::new();
        if let Some(fg) = self.fg {
            classes.push(format!("fg{fg}"));
        }
        for (on, class) in [
            (self.bold, "bold"),
            (self.dim, "dim"),
            (self.italic, "italic"),
            (self.underline, "underline"),
        ] {
            if on {
                classes.push(class.to_owned());
            }
        }
        classes.join(" ")
    }

    /// Applies the SGR parameters `params` (the text between `ESC [` and
    /// `m`). Panics on anything else, so that the site never shows an escape
    /// it doesn't style.
    fn apply(&mut self, params: &str) {
        let codes: Vec<u8> = params
            .split(';')
            .map(|code| {
                if code.is_empty() {
                    0
                } else {
                    code.parse().unwrap()
                }
            })
            .collect();
        for code in codes {
            match code {
                0 => *self = Self::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                22 => (self.bold, self.dim) = (false, false),
                23 => self.italic = false,
                24 => self.underline = false,
                30..=37 => self.fg = Some(code - 30),
                39 => self.fg = None,
                90..=97 => self.fg = Some(code - 90 + 8),
                _ => panic!("unsupported ANSI SGR code {code} in {params:?}"),
            }
        }
    }
}

/// `text` with its ANSI SGR escapes turned into HTML spans (see [`Style`]),
/// escaped for HTML.
fn ansi_to_html(text: &str) -> String {
    fn push(out: &mut String, piece: &str, style: Style) {
        if piece.is_empty() {
        } else if style == Style::default() {
            out.push_str(&escape(piece));
        } else {
            let classes = style.classes();
            out.push_str(&format!(
                "<span class=\"{classes}\">{}</span>",
                escape(piece)
            ));
        }
    }
    let mut out = String::new();
    let mut style = Style::default();
    let mut rest = text;
    while let Some(start) = rest.find('\x1b') {
        push(&mut out, &rest[..start], style);
        let escape_sequence = &rest[start..];
        assert!(
            escape_sequence.starts_with("\x1b["),
            "unsupported escape in {text:?}"
        );
        let end = escape_sequence.find('m').unwrap();
        style.apply(&escape_sequence[2..end]);
        rest = &escape_sequence[end + 1..];
    }
    push(&mut out, rest, style);
    out
}

/// A temp `HEARTY_CACHE_DIR` holding a fresh version cache listing
/// [`CACHED_BRANCHES`].
fn seeded_cache() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let branches: serde_json::Map<String, Value> = CACHED_BRANCHES
        .iter()
        .map(|branch| ((*branch).to_owned(), json!({})))
        .collect();
    let cache = json!({
        "data": { "394360": { "depots": { "branches": branches } } },
        "fetched_at_secs": now,
    });
    std::fs::write(
        dir.path().join("hoi4-version-cache.json"),
        cache.to_string(),
    )
    .unwrap();
    dir
}

/// What `hearty --rules-json` prints.
fn rules_json() -> Value {
    let output = Command::new(BINARY).arg("--rules-json").output().unwrap();
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

/// `prose` with each `{{name}}` replaced by the constant `name`.
fn fill(prose: &str, constants: &Value) -> String {
    let mut out = prose.to_owned();
    for (name, value) in constants.as_object().unwrap() {
        out = out.replace(&format!("{{{{{name}}}}}"), &value.to_string());
    }
    assert!(!out.contains("{{"), "unknown constant in {out:?}");
    out
}

/// The summary's `Changes:` line in `output`, its changes listed.
fn changes(output: &str) -> Vec<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("Changes: "))
        .map(|line| line.split(", ").map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Runs `example` and checks it does what it says (see [`Example`]),
/// returning it as the site shows it.
fn run_example(example: &Example, cache: &Path) -> Value {
    let files = example.files.iter().map(|file| (file.path, file.text));
    let mod_ = Mod::new(files);
    let command = format!("hearty {}", example.args.join(" "));
    let output = if example.diagnostics_only {
        lint_run(hearty(&mod_.root, example.args, cache)).1
    } else {
        run(hearty(&mod_.root, example.args, cache)).output
    };
    let text = plain(&output);
    for expected in example.expect {
        assert!(
            text.contains(expected),
            "{command}: no {expected:?} in\n{text}"
        );
    }
    let formats = example.args.contains(&"--format");
    let writes = formats || example.args.contains(&"--fix");
    if let Some(expected) = example.changes {
        assert_eq!(changes(&text), expected, "{command}:\n{text}");
        if expected.is_empty() {
            assert!(
                text.contains("all files already formatted") || text.contains("Nothing to fix"),
                "{command}:\n{text}"
            );
        }
    }
    if formats {
        // What `--format` wrote is formatted: `--check` with the same
        // options passes.
        let args: Vec<&str> = example
            .args
            .iter()
            .map(|&arg| if arg == "--format" { "--check" } else { arg })
            .collect();
        let check = run(hearty(&mod_.root, &args, cache));
        assert!(
            check.success,
            "{command} is not idempotent:\n{}",
            check.output
        );
    }
    let files: Vec<Value> = example
        .files
        .iter()
        .filter(|file| file.shown)
        .map(|file| {
            let after = mod_.read(file.path);
            if !writes {
                assert_eq!(after, file.text, "{command} changed {}", file.path);
            }
            json!({
                "after": writes.then_some(after),
                "before": file.text,
                "path": file.path,
            })
        })
        .collect();
    json!({
        "after_command": writes.then_some(&command),
        "command": command,
        "files": files,
        "output": ansi_to_html(&output),
        "title": (!example.title.is_empty()).then_some(example.title),
    })
}

/// Runs the example of the redundant-field `rule` (from `hearty
/// --rules-json`): the field in a block of the first of
/// [`PREFERRED_KINDS`] it applies to, written from [`TEMPLATES`]. Checks
/// that `--lint` reports it and nothing else, and that `--fix` removes its
/// line and nothing else.
fn redundant_example(rule: &Value, kinds: &BTreeMap<String, Value>, cache: &Path) -> Value {
    let id = rule["id"].as_str().unwrap();
    let key = rule["key"].as_str().unwrap();
    let rule_kinds: Vec<&str> = rule["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| kind.as_str().unwrap())
        .collect();
    let kind = PREFERRED_KINDS
        .iter()
        .find(|kind| rule_kinds.contains(kind))
        .unwrap_or_else(|| panic!("{id}: add its kinds of block to PREFERRED_KINDS"));
    let template = TEMPLATES
        .iter()
        .find(|template| template.kind == *kind)
        .unwrap_or_else(|| panic!("{id}: add a template for {kind} to TEMPLATES"));
    let value = rule["value"]
        .as_str()
        .map_or_else(|| template.owner.to_owned(), str::to_owned);
    assert!(!value.is_empty(), "{id}: give the {kind} template an owner");
    let field = format!("{key} = {value}");

    // The field goes before the first field that comes after it in the
    // kind's order (fields not in it come last), replacing any field of the
    // same key.
    let order: Vec<&str> = kinds[*kind]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field.as_str().unwrap())
        .collect();
    let rank = |key: &str| {
        order
            .iter()
            .position(|&field| field == key)
            .unwrap_or(usize::MAX)
    };
    let mut lines: Vec<&str> = Vec::new();
    let mut placed = false;
    for &(other, text) in template.fields.iter().filter(|(other, _)| *other != key) {
        if !placed && rank(other) > rank(key) {
            lines.push(&field);
            placed = true;
        }
        lines.push(text);
    }
    if !placed {
        lines.push(&field);
    }
    let body: String = lines
        .iter()
        .map(|line| format!("{}{line}\n", template.indent))
        .collect();
    let before = format!("{}{body}{}", template.head, template.tail);
    let line = format!("{}{field}\n", template.indent);
    let expected_after = before.replacen(&line, "", 1);
    assert_ne!(before, expected_after);

    let mut files = vec![(template.path, before.as_str())];
    if !template.localisation.is_empty() {
        files.push((LOCALISATION, template.localisation));
    }
    let linted = Mod::new(files.iter().copied());
    let (stdout, diagnostics, closing) = lint_run(hearty(&linted.root, &["--lint"], cache));
    let text = plain(&format!("{stdout}{diagnostics}{closing}"));
    let explanation = rule["explanation"].as_str().unwrap();
    for expected in [
        format!("`{field}` has no effect: {explanation}."),
        "Found 0/".to_owned(),
        "Found 1 redundant fields (1 fixable with --fix).".to_owned(),
        "lint found 1 problem(s)".to_owned(),
    ] {
        assert!(text.contains(&expected), "{id}: no {expected:?} in\n{text}");
    }

    let fixed = Mod::new(files.iter().copied());
    let fix = run(hearty(&fixed.root, &["--fix"], cache));
    assert!(fix.success, "{id}:\n{}", fix.output);
    assert_eq!(changes(&fix.output), ["1 redundant field removed"], "{id}");
    let after = fixed.read(template.path);
    assert_eq!(after, expected_after, "{id}");

    json!({
        "after_command": "hearty --fix",
        "command": "hearty --lint",
        "files": [{ "after": after, "before": before, "path": template.path }],
        "output": ansi_to_html(&diagnostics),
        "title": null,
    })
}

/// The kinds of block the redundant-field `rule` applies to, in prose: `a
/// decision or an event`.
fn kinds_phrase(rule: &Value, kinds: &BTreeMap<String, Value>) -> String {
    let names: Vec<String> = rule["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| {
            let name = kinds[kind.as_str().unwrap()]["name"]
                .as_str()
                .unwrap()
                .to_lowercase();
            let article = if name.starts_with(['a', 'e', 'i', 'o', 'u']) {
                "an"
            } else {
                "a"
            };
            format!("{article} {name}")
        })
        .collect();
    match names.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => names.concat(),
    }
}

/// The site's entry for the redundant-field `rule` (from `hearty
/// --rules-json`).
fn redundant_rule(rule: &Value, kinds: &BTreeMap<String, Value>, cache: &Path) -> Value {
    let key = rule["key"].as_str().unwrap();
    let field = rule["value"].as_str().map_or_else(
        || format!("{key} = <block name>"),
        |value| format!("{key} = {value}"),
    );
    let explanation = rule["explanation"].as_str().unwrap();
    let mut why = explanation.to_owned();
    if let Some(first) = why.get(..1) {
        why.replace_range(..1, &first.to_uppercase());
    }
    let notes = if rule["repeatable"].as_bool().unwrap() {
        format!("`{key}` may appear several times in a block: each redundant one is reported.")
    } else {
        format!(
            "Only reported in a block with a single `{key}`: removing one of several could \
             change which one the game uses."
        )
    };
    let blocks = kinds_phrase(rule, kinds);
    let what = if rule["value_kind"] == "own_name" {
        format!(
            "Reports a `{key}` set to the name of the block it's in, in {blocks} with no \
             `name` field."
        )
    } else {
        format!("Reports the field `{field}` in {blocks}.")
    };
    json!({
        "applies_to": rule["kinds"],
        "category": "lint",
        "examples": [redundant_example(rule, kinds, cache)],
        "fix": true,
        "group": "redundant",
        "id": rule["id"],
        "notes": notes,
        "summary": format!("{why}."),
        "title": format!("`{field}`"),
        "what": what,
        "why": format!("{why}, so the field changes nothing: it only makes the block longer, \
                        and a reader may take it to matter."),
    })
}

/// The site's entry for a rule described in [`RULES`].
fn described_rule(rule: &Rule, constants: &Value, cache: &Path) -> Value {
    let category = GROUPS
        .iter()
        .find(|group| group.id == rule.group)
        .unwrap()
        .category;
    let examples: Vec<Value> = rule
        .examples
        .par_iter()
        .map(|example| run_example(example, cache))
        .collect();
    json!({
        "applies_to": [],
        "category": category,
        "examples": examples,
        "fix": rule.fix,
        "group": rule.group,
        "id": rule.id,
        "notes": fill(rule.notes, constants),
        "summary": fill(rule.summary, constants),
        "title": rule.title,
        "what": fill(rule.what, constants),
        "why": fill(rule.why, constants),
    })
}

/// The overview's example: `hearty my_mod` run beside a fake Hearts of Iron
/// IV (see `tests/fake_game`), showing all its output.
fn overview(cache: &Path) -> Value {
    let mod_ = Mod::new(OVERVIEW.iter().map(|file| (file.path, file.text)));
    let parent = mod_.root.parent().unwrap();
    copy_dir::copy_dir("tests/fake_game", parent.join("Hearts of Iron IV")).unwrap();
    let mut command = hearty(parent, &["my_mod"], cache);
    command.env("HEARTY_GAME_DIR", "Hearts of Iron IV");
    let result = run(command);
    assert!(!result.success);
    let text = plain(&result.output);
    for expected in [
        "does not match latest HOI4 1.17.3.",
        "\"example.1.a\" not localised in: english.",
        "`fire_only_once = no` has no effect",
        "Read 2 localisation files of the base game from Hearts of Iron IV.",
    ] {
        assert!(text.contains(expected), "no {expected:?} in\n{text}");
    }
    json!({
        "after_command": null,
        "command": "hearty my_mod",
        "files": OVERVIEW.iter().map(|file| json!({
            "after": null,
            "before": file.text,
            "path": file.path,
        })).collect::<Vec<_>>(),
        "output": ansi_to_html(&result.output),
        "title": null,
    })
}

/// The options (from `hearty --rules-json`) with the details and examples
/// described here.
fn options(code: &Value, cache: &Path) -> Vec<Value> {
    let details: BTreeMap<&str, &str> = OPTION_DETAILS.iter().copied().collect();
    let examples: BTreeMap<&str, &[Example]> = OPTION_EXAMPLES.iter().copied().collect();
    let options = code["options"].as_array().unwrap();
    let ids: BTreeSet<&str> = options
        .iter()
        .map(|option| option["id"].as_str().unwrap())
        .collect();
    for id in details.keys().chain(examples.keys()) {
        assert!(ids.contains(id), "no option {id}");
    }
    options
        .iter()
        .map(|option| {
            let id = option["id"].as_str().unwrap();
            let mut option = option.clone();
            option["details"] = json!(details.get(id));
            option["examples"] = examples
                .get(id)
                .map(|examples| {
                    examples
                        .par_iter()
                        .map(|example| run_example(example, cache))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
                .into();
            option
        })
        .collect()
}

/// The environment variables (from `hearty --rules-json`), described.
fn environment(code: &Value) -> Vec<Value> {
    let described: BTreeMap<&str, &str> = ENVIRONMENT.iter().copied().collect();
    let variables = code["environment"].as_array().unwrap();
    assert_eq!(
        variables.len(),
        described.len(),
        "describe every variable in ENVIRONMENT"
    );
    variables
        .iter()
        .map(|variable| {
            let name = variable["name"].as_str().unwrap();
            let mut variable = variable.clone();
            variable["description"] = json!(
                described
                    .get(name)
                    .unwrap_or_else(|| panic!("describe {name} in ENVIRONMENT"))
            );
            variable
        })
        .collect()
}

/// `hearty --help`, in colour.
fn usage() -> String {
    let mut command = Command::new(BINARY);
    command
        .arg("--help")
        .env("CLICOLOR_FORCE", "1")
        .env_remove("NO_COLOR");
    let result = run(command);
    assert!(result.success);
    ansi_to_html(&result.output)
}

/// The site's data: everything the page shows.
fn site_data(cache: &Path) -> Value {
    let code = rules_json();
    let constants = &code["constants"];
    let kinds: BTreeMap<String, Value> = code["block_kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| (kind["id"].as_str().unwrap().to_owned(), kind.clone()))
        .collect();
    let described = |category: &str| -> Vec<Value> {
        RULES
            .par_iter()
            .filter(|rule| {
                GROUPS
                    .iter()
                    .any(|group| group.id == rule.group && group.category == category)
            })
            .map(|rule| described_rule(rule, constants, cache))
            .collect()
    };
    let mut rules = described("lint");
    rules.extend(
        code["redundant_rules"]
            .as_array()
            .unwrap()
            .par_iter()
            .map(|rule| redundant_rule(rule, &kinds, cache))
            .collect::<Vec<_>>(),
    );
    rules.extend(described("format"));
    let groups: Vec<Value> = GROUPS
        .iter()
        .map(|group| {
            json!({
                "category": group.category,
                "id": group.id,
                "intro": fill(group.intro, constants),
                "name": group.name,
            })
        })
        .collect();
    json!({
        "block_kinds": code["block_kinds"],
        "constants": constants,
        "environment": environment(&code),
        "groups": groups,
        "options": options(&code, cache),
        "overview": overview(cache),
        "rules": rules,
        "usage": usage(),
    })
}

/// The site's data file holding `data`.
fn data_file(data: &Value) -> String {
    format!(
        "// The rules site's data, generated by tests/docs.rs from hearty's code and output: do \
         not edit.\n// Regenerate it with `HEARTY_BLESS=1 cargo test --test docs`.\n\
         window.HEARTY = {};\n",
        serde_json::to_string_pretty(data).unwrap()
    )
}

/// Every anchor the page has for `data`: the ids of `page` (the HTML of
/// `docs/index.html`) and those `docs/app.js` makes for the data, each in
/// the order made.
fn anchors(data: &Value, page: &str) -> Vec<String> {
    let mut anchors: Vec<String> = page
        .split(" id=\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap().to_owned())
        .collect();
    let each = |key: &str| data[key].as_array().unwrap().iter();
    anchors.extend(each("groups").map(|group| format!("group-{}", group["id"].as_str().unwrap())));
    anchors.extend(each("rules").map(|rule| rule["id"].as_str().unwrap().to_owned()));
    anchors.extend(
        each("block_kinds").map(|kind| format!("field_order_{}", kind["id"].as_str().unwrap())),
    );
    anchors.extend(each("options").map(|option| {
        option["long"].as_str().map_or_else(
            || option["id"].as_str().unwrap().to_owned(),
            |long| format!("--{long}"),
        )
    }));
    anchors
        .extend(each("environment").map(|variable| variable["name"].as_str().unwrap().to_owned()));
    anchors
}

/// The `#id` targets of the links in the Markdown `text`.
fn link_targets(text: &str) -> Vec<String> {
    text.split("](#")
        .skip(1)
        .map(|rest| rest.split(')').next().unwrap().to_owned())
        .collect()
}

/// The targets of the `href="#id"` links written out in full in `html` (or
/// in the strings of a script making HTML): not those whose id a script
/// adds, as in `'href="#' + id`.
fn href_targets(html: &str) -> Vec<String> {
    html.split("href=\"#")
        .skip(1)
        .filter_map(|rest| {
            let end = rest.find(['"', '\''])?;
            rest[end..].starts_with('"').then(|| rest[..end].to_owned())
        })
        .collect()
}

/// Checks that no anchor of the page is made twice, and that every link to
/// one leads somewhere: those of the page, of its script, of the prose in
/// `data`, and those of [`LINKING_FILES`] into the site (as the README's to
/// its sections).
fn check_links(data: &Value) {
    let page = std::fs::read_to_string(PAGE_FILE).unwrap();
    let made = anchors(data, &page);
    let anchors: BTreeSet<&String> = made.iter().collect();
    assert_eq!(
        anchors.len(),
        made.len(),
        "an anchor is made twice: {made:?}"
    );

    let mut prose = String::new();
    collect_strings(data, &mut prose);
    let script = std::fs::read_to_string(SCRIPT_FILE).unwrap();
    let mut links: Vec<(String, String)> = link_targets(&prose)
        .into_iter()
        .map(|target| ("the site's data".to_owned(), target))
        .chain(
            href_targets(&page)
                .into_iter()
                .map(|target| (PAGE_FILE.to_owned(), target)),
        )
        .chain(
            href_targets(&script)
                .into_iter()
                .map(|target| (SCRIPT_FILE.to_owned(), target)),
        )
        .collect();
    for file in LINKING_FILES {
        let text = std::fs::read_to_string(file).unwrap();
        assert!(
            text.contains(SITE_URL),
            "{file} doesn't link to the site at {SITE_URL}: has it moved?"
        );
        let from_site = format!("{SITE_URL}#");
        links.extend(text.split(&from_site).skip(1).map(|rest| {
            let end = rest.find([')', '"', '>', ' ']).unwrap_or(rest.len());
            ((*file).to_owned(), rest[..end].to_owned())
        }));
    }
    for (file, target) in links {
        assert!(
            target.is_empty() || anchors.contains(&target),
            "a link in {file} leads to #{target}, which the page doesn't have"
        );
    }
}

/// Appends every string in `value` to `out`.
fn collect_strings(value: &Value, out: &mut String) {
    match value {
        Value::String(text) => {
            out.push_str(text);
            out.push('\n');
        }
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, out)),
        Value::Object(fields) => fields
            .values()
            .for_each(|field| collect_strings(field, out)),
        Value::Bool(_) | Value::Null | Value::Number(_) => {}
    }
}

/// The rules site's data is what the code and hearty's output make it; see
/// the module docs. With [`BLESS_VAR`] set to `1`, writes it instead.
#[test]
fn rules_site_is_up_to_date() {
    // The published crate leaves `docs/` out (see `exclude` in `Cargo.toml`).
    if !Path::new(PAGE_FILE).is_file() {
        println!("{PAGE_FILE} is missing (as in the published crate): nothing to check");
        return;
    }
    let cache = seeded_cache();
    let data = site_data(cache.path());
    check_links(&data);
    let expected = data_file(&data);
    let path = Path::new(DATA_FILE);
    let committed = std::fs::read_to_string(path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    if committed == expected {
        return;
    }
    if std::env::var(BLESS_VAR).is_ok_and(|value| value == "1") {
        std::fs::write(path, &expected).unwrap();
        return;
    }
    let line = committed
        .lines()
        .zip(expected.lines())
        .position(|(committed, expected)| committed != expected)
        .unwrap_or_else(|| committed.lines().count().min(expected.lines().count()));
    panic!(
        "{DATA_FILE} is out of date (first difference on line {}): regenerate it with \
         `{BLESS_VAR}=1 cargo test --test docs`\n  committed: {:?}\n  generated: {:?}",
        line + 1,
        committed.lines().nth(line).unwrap_or_default(),
        expected.lines().nth(line).unwrap_or_default(),
    );
}

/// Every lint diagnostic (a struct deriving miette's `Diagnostic`) and every
/// kind of formatting change (a variant of `Change` in `src/report.rs`) in
/// `src/` is described by a rule of the site.
#[test]
fn every_lint_and_change_is_documented() {
    // Redundant fields are the rules the code describes; their fix is
    // theirs.
    let documented: BTreeSet<&str> = RULES
        .iter()
        .flat_map(|rule| rule.code.iter().copied())
        .chain(["RedundantField", "RedundantRemoved"])
        .collect();
    let mut found = BTreeSet::new();
    for entry in std::fs::read_dir("src").unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut deriving = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with("#[derive(") && line.contains("Diagnostic") {
                deriving = true;
            } else if deriving
                && let Some(rest) = line
                    .strip_prefix("pub struct ")
                    .or_else(|| line.strip_prefix("struct "))
            {
                let name = rest.split([' ', '{', '<', ';']).next().unwrap();
                found.insert(name.to_owned());
                deriving = false;
            }
        }
    }
    let report = std::fs::read_to_string("src/report.rs").unwrap();
    let body = report
        .split("pub enum Change {")
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap();
    found.extend(
        body.lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//") && line.ends_with(','))
            .map(|line| line.trim_end_matches(',').to_owned()),
    );
    assert!(found.contains("MissingLocalisation") && found.contains("SpacingFixed"));
    let undocumented: Vec<&String> = found
        .iter()
        .filter(|name| !documented.contains(name.as_str()))
        .collect();
    assert!(
        undocumented.is_empty(),
        "describe {undocumented:?} in a rule of tests/docs.rs (see its `code`)"
    );
    let stale: Vec<&&str> = documented
        .iter()
        .filter(|name| !found.contains(**name))
        .collect();
    assert!(stale.is_empty(), "{stale:?} are no longer in src/");
}

#[test]
fn ansi_escapes_become_spans() {
    assert_eq!(
        ansi_to_html("\x1b[33m⚠\x1b[0m a <b>\n\x1b[36;1;4mx\ny\x1b[0m"),
        "<span class=\"fg3\">⚠</span> a &lt;b&gt;\n<span class=\"fg6 bold underline\">x\ny</span>"
    );
    assert_eq!(plain("\x1b[2m1\x1b[0m │"), "1 │");
}
