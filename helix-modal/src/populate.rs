//! Populates the [`CommandRegistry`] with all extracted commands.
//!
//! Each command is classified as Motion, Operator, TextObject, Action, or CharPending
//! based on its semantics. The engine uses this classification to apply the correct
//! composition rules (e.g., operator-pending × motion in Vim mode).

use helix_core::movement::{Direction, Movement};
use helix_core::textobject;
use helix_view::commands::{editing, movement as mv};
use helix_view::engine::{
    ActionId, CharPendingId, MotionFn, MotionId, OperatorId, TextObjectFn, TextObjectId,
};
use helix_view::{DocumentId, ViewId};
use std::num::NonZeroUsize;
use std::sync::OnceLock;

use crate::registry::{
    ActionEntry, CharPendingEntry, CharPendingResolution, CommandKind, CommandRegistry,
    CommandRegistryBuilder, CommandScope, EngineCommandSpec, MotionEntry, MotionFactory,
    OperatorEntry, TextObjectEntry,
};

trait EngineCommandCatalog {
    fn motion(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        make: fn(usize) -> MotionFn,
    );

    fn motion_optional(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        make: fn(Option<NonZeroUsize>) -> MotionFn,
    );

    fn operator(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        execute: fn(&mut helix_view::Editor, ViewId, DocumentId, Option<char>),
    );

    fn text_object(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        make: fn(usize) -> TextObjectFn,
    );

    fn action(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        execute: fn(&mut helix_view::Editor, ViewId, DocumentId, usize, Option<char>),
    );

    fn char_pending(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        resolve: fn(char, usize) -> MotionFn,
    );
}

#[derive(Default)]
struct SpecCatalogBuilder {
    specs: Vec<EngineCommandSpec>,
}

impl SpecCatalogBuilder {
    fn freeze(self) -> Box<[EngineCommandSpec]> {
        self.specs.into_boxed_slice()
    }
}

impl EngineCommandCatalog for SpecCatalogBuilder {
    fn motion(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        _make: fn(usize) -> MotionFn,
    ) {
        self.specs.push(EngineCommandSpec::motion(name, doc, scope));
    }

    fn motion_optional(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        _make: fn(Option<NonZeroUsize>) -> MotionFn,
    ) {
        self.specs.push(EngineCommandSpec::motion(name, doc, scope));
    }

    fn operator(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        _execute: fn(&mut helix_view::Editor, ViewId, DocumentId, Option<char>),
    ) {
        self.specs
            .push(EngineCommandSpec::operator(name, doc, scope));
    }

    fn text_object(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        _make: fn(usize) -> TextObjectFn,
    ) {
        self.specs
            .push(EngineCommandSpec::text_object(name, doc, scope));
    }

    fn action(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        _execute: fn(&mut helix_view::Editor, ViewId, DocumentId, usize, Option<char>),
    ) {
        self.specs.push(EngineCommandSpec::action(name, doc, scope));
    }

    fn char_pending(
        &mut self,
        name: &'static str,
        doc: &'static str,
        scope: CommandScope,
        _resolve: fn(char, usize) -> MotionFn,
    ) {
        self.specs
            .push(EngineCommandSpec::char_pending(name, doc, scope));
    }
}

#[derive(Default)]
struct RegistryCatalogBuilder {
    registry: CommandRegistryBuilder,
}

impl RegistryCatalogBuilder {
    fn freeze(self) -> CommandRegistry {
        self.registry.freeze()
    }
}

impl EngineCommandCatalog for RegistryCatalogBuilder {
    fn motion(
        &mut self,
        name: &'static str,
        _doc: &'static str,
        _scope: CommandScope,
        make: fn(usize) -> MotionFn,
    ) {
        self.registry.register(CommandKind::Motion(MotionEntry {
            id: MotionId::new(name),
            make: MotionFactory::Counted(make),
        }));
    }

    fn motion_optional(
        &mut self,
        name: &'static str,
        _doc: &'static str,
        _scope: CommandScope,
        make: fn(Option<NonZeroUsize>) -> MotionFn,
    ) {
        self.registry.register(CommandKind::Motion(MotionEntry {
            id: MotionId::new(name),
            make: MotionFactory::Optional(make),
        }));
    }

    fn operator(
        &mut self,
        name: &'static str,
        _doc: &'static str,
        _scope: CommandScope,
        execute: fn(&mut helix_view::Editor, ViewId, DocumentId, Option<char>),
    ) {
        self.registry.register(CommandKind::Operator(OperatorEntry {
            id: OperatorId::new(name),
            execute,
        }));
    }

    fn text_object(
        &mut self,
        name: &'static str,
        _doc: &'static str,
        _scope: CommandScope,
        make: fn(usize) -> TextObjectFn,
    ) {
        self.registry
            .register(CommandKind::TextObject(TextObjectEntry {
                id: TextObjectId::new(name),
                make,
            }));
    }

    fn action(
        &mut self,
        name: &'static str,
        _doc: &'static str,
        _scope: CommandScope,
        execute: fn(&mut helix_view::Editor, ViewId, DocumentId, usize, Option<char>),
    ) {
        self.registry.register(CommandKind::Action(ActionEntry {
            id: ActionId::new(name),
            execute,
        }));
    }

    fn char_pending(
        &mut self,
        name: &'static str,
        _doc: &'static str,
        _scope: CommandScope,
        resolve: fn(char, usize) -> MotionFn,
    ) {
        self.registry
            .register(CommandKind::CharPending(CharPendingEntry {
                id: CharPendingId::new(name),
                resolve: Box::new(move |ch, count| {
                    CharPendingResolution::Motion(resolve(ch, count))
                }),
            }));
    }
}

fn register_commands(catalog: &mut impl EngineCommandCatalog) {
    register_motions(catalog);
    register_operators(catalog);
    register_text_objects(catalog);
    register_actions(catalog);
    register_char_pending(catalog);
}

pub fn engine_command_specs() -> &'static [EngineCommandSpec] {
    static ENGINE_COMMAND_SPECS: OnceLock<Box<[EngineCommandSpec]>> = OnceLock::new();
    ENGINE_COMMAND_SPECS.get_or_init(|| {
        let mut catalog = SpecCatalogBuilder::default();
        register_commands(&mut catalog);
        catalog.freeze()
    })
}

impl CommandRegistry {
    /// Build the registry for Helix's built-in modal commands.
    #[must_use]
    pub fn builtins() -> Self {
        let mut catalog = RegistryCatalogBuilder::default();
        register_commands(&mut catalog);
        catalog.freeze()
    }
}

// ─── Motions ─────────────────────────────────────────────────────────

fn register_motions(catalog: &mut impl EngineCommandCatalog) {
    // Directional motions — use canonical Movement-parameterized functions
    catalog.motion(
        "move_char_left",
        "Move left",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::char_left(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_char_right",
        "Move right",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::char_right(ed, vid, did, count, m)),
    );
    catalog.motion("move_line_up", "Move up", CommandScope::Viewport, |count| {
        Box::new(move |ed, vid, did, m| mv::line_up(ed, vid, did, count, m))
    });
    catalog.motion(
        "move_line_down",
        "Move down",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::line_down(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_visual_line_up",
        "Move up",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::visual_line_up(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_visual_line_down",
        "Move down",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::visual_line_down(ed, vid, did, count, m)),
    );

    // Word motions
    catalog.motion(
        "move_next_word_start",
        "Move to start of next word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::next_word_start(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_prev_word_start",
        "Move to start of previous word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::prev_word_start(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_next_word_end",
        "Move to end of next word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::next_word_end(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_prev_word_end",
        "Move to end of previous word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::prev_word_end(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_next_long_word_start",
        "Move to start of next long word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::next_long_word_start(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_prev_long_word_start",
        "Move to start of previous long word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::prev_long_word_start(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_next_long_word_end",
        "Move to end of next long word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::next_long_word_end(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_prev_long_word_end",
        "Move to end of previous long word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::prev_long_word_end(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_next_sub_word_start",
        "Move to start of next sub word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::next_sub_word_start(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_prev_sub_word_start",
        "Move to start of previous sub word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::prev_sub_word_start(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_next_sub_word_end",
        "Move to end of next sub word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::next_sub_word_end(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_prev_sub_word_end",
        "Move to end of previous sub word",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::prev_sub_word_end(ed, vid, did, count, m)),
    );
    catalog.motion(
        "move_parent_node_end",
        "Move to end of the parent node",
        CommandScope::Viewport,
        |_count| {
            Box::new(|ed, vid, did, m| {
                editing::move_node_bound(ed, vid, did, Direction::Forward, m)
            })
        },
    );
    catalog.motion(
        "move_parent_node_start",
        "Move to beginning of the parent node",
        CommandScope::Viewport,
        |_count| {
            Box::new(|ed, vid, did, m| {
                editing::move_node_bound(ed, vid, did, Direction::Backward, m)
            })
        },
    );

    // Extend variants — always force Movement::Extend regardless of engine's movement.
    // These exist so keymaps can bind extend_char_left to always extend, even in normal mode.
    catalog.motion(
        "extend_char_left",
        "Extend left",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| mv::char_left(ed, vid, did, count, Movement::Extend))
        },
    );
    catalog.motion(
        "extend_char_right",
        "Extend right",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| mv::char_right(ed, vid, did, count, Movement::Extend))
        },
    );
    catalog.motion(
        "extend_line_up",
        "Extend up",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| mv::line_up(ed, vid, did, count, Movement::Extend))
        },
    );
    catalog.motion(
        "extend_line_down",
        "Extend down",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| mv::line_down(ed, vid, did, count, Movement::Extend))
        },
    );
    catalog.motion(
        "extend_visual_line_up",
        "Extend up",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::visual_line_up(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_visual_line_down",
        "Extend down",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::visual_line_down(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_next_word_start",
        "Extend to start of next word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::next_word_start(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_prev_word_start",
        "Extend to start of previous word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::prev_word_start(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_next_word_end",
        "Extend to end of next word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::next_word_end(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_prev_word_end",
        "Extend to end of previous word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::prev_word_end(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_next_long_word_start",
        "Extend to start of next long word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::next_long_word_start(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_prev_long_word_start",
        "Extend to start of previous long word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::prev_long_word_start(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_next_long_word_end",
        "Extend to end of next long word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::next_long_word_end(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_prev_long_word_end",
        "Extend to end of prev long word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::prev_long_word_end(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_next_sub_word_start",
        "Extend to start of next sub word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::next_sub_word_start(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_prev_sub_word_start",
        "Extend to start of previous sub word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::prev_sub_word_start(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_next_sub_word_end",
        "Extend to end of next sub word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::next_sub_word_end(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_prev_sub_word_end",
        "Extend to end of prev sub word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::prev_sub_word_end(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_parent_node_end",
        "Extend to end of the parent node",
        CommandScope::Viewport,
        |_count| {
            Box::new(|ed, vid, did, _m| {
                editing::move_node_bound(ed, vid, did, Direction::Forward, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_parent_node_start",
        "Extend to beginning of the parent node",
        CommandScope::Viewport,
        |_count| {
            Box::new(|ed, vid, did, _m| {
                editing::move_node_bound(ed, vid, did, Direction::Backward, Movement::Extend)
            })
        },
    );

    // Line boundary motions (ignore count, engine passes Movement)
    catalog.motion(
        "goto_line_end",
        "Goto line end",
        CommandScope::Viewport,
        |_count| Box::new(mv::goto_line_end_with_movement),
    );
    catalog.motion(
        "goto_line_end_newline",
        "Goto newline at line end",
        CommandScope::Viewport,
        |_count| Box::new(mv::goto_line_end_newline_with_movement),
    );
    catalog.motion(
        "goto_line_start",
        "Goto line start",
        CommandScope::Viewport,
        |_count| Box::new(mv::goto_line_start_with_movement),
    );
    catalog.motion(
        "goto_first_nonwhitespace",
        "Goto first non-blank in line",
        CommandScope::Viewport,
        |_count| Box::new(mv::goto_first_nonwhitespace_with_movement),
    );
    catalog.motion(
        "extend_to_line_end",
        "Extend to line end",
        CommandScope::Viewport,
        |_count| {
            Box::new(|ed, vid, did, _m| {
                mv::goto_line_end_with_movement(ed, vid, did, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_to_line_end_newline",
        "Extend to line end",
        CommandScope::Viewport,
        |_count| {
            Box::new(|ed, vid, did, _m| {
                mv::goto_line_end_newline_with_movement(ed, vid, did, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_to_line_start",
        "Extend to line start",
        CommandScope::Viewport,
        |_count| {
            Box::new(|ed, vid, did, _m| {
                mv::goto_line_start_with_movement(ed, vid, did, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_to_first_nonwhitespace",
        "Extend to first non-blank in line",
        CommandScope::Viewport,
        |_count| {
            Box::new(|ed, vid, did, _m| {
                mv::goto_first_nonwhitespace_with_movement(ed, vid, did, Movement::Extend)
            })
        },
    );

    // Paragraph motions
    catalog.motion(
        "goto_prev_paragraph",
        "Goto previous paragraph",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, _m| mv::goto_prev_paragraph(ed, vid, did, count)),
    );
    catalog.motion(
        "goto_next_paragraph",
        "Goto next paragraph",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, _m| mv::goto_next_paragraph(ed, vid, did, count)),
    );

    // File/line position motions
    catalog.motion_optional(
        "goto_file_start",
        "Goto line number <n> else file start",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::goto_file_start(ed, vid, did, count, m)),
    );
    catalog.motion(
        "goto_file_end",
        "Goto file end",
        CommandScope::Viewport,
        |_count| Box::new(mv::goto_file_end),
    );
    catalog.motion_optional("goto_line", "Goto line", CommandScope::Viewport, |count| {
        Box::new(move |ed, vid, did, m| mv::goto_line(ed, vid, did, count, m))
    });
    catalog.motion(
        "goto_last_line",
        "Goto last line",
        CommandScope::Viewport,
        |_count| Box::new(mv::goto_last_line),
    );
    catalog.motion_optional(
        "extend_to_file_start",
        "Extend to line number<n> else file start",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| {
                mv::goto_file_start(ed, vid, did, count, Movement::Extend)
            })
        },
    );
    catalog.motion(
        "extend_to_file_end",
        "Extend to file end",
        CommandScope::Viewport,
        |_count| Box::new(|ed, vid, did, _m| mv::goto_file_end(ed, vid, did, Movement::Extend)),
    );
    catalog.motion(
        "extend_to_last_line",
        "Extend to last line",
        CommandScope::Viewport,
        |_count| Box::new(|ed, vid, did, _m| mv::goto_last_line(ed, vid, did, Movement::Extend)),
    );
    catalog.motion(
        "goto_column",
        "Goto column",
        CommandScope::Viewport,
        |count| Box::new(move |ed, vid, did, m| mv::goto_column(ed, vid, did, count, m)),
    );
    catalog.motion(
        "extend_to_column",
        "Extend to column",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, _m| mv::goto_column(ed, vid, did, count, Movement::Extend))
        },
    );

    // Tree-sitter object motions (registered as Actions — they don't compose with operators)
    catalog.action(
        "goto_next_function",
        "Goto next function",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "function", Direction::Forward, count)
        },
    );
    catalog.action(
        "goto_prev_function",
        "Goto previous function",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "function", Direction::Backward, count)
        },
    );
    catalog.action(
        "goto_next_class",
        "Goto next type definition",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "class", Direction::Forward, count)
        },
    );
    catalog.action(
        "goto_prev_class",
        "Goto previous type definition",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "class", Direction::Backward, count)
        },
    );
    catalog.action(
        "goto_next_parameter",
        "Goto next parameter",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "parameter", Direction::Forward, count)
        },
    );
    catalog.action(
        "goto_prev_parameter",
        "Goto previous parameter",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "parameter", Direction::Backward, count)
        },
    );
    catalog.action(
        "goto_next_comment",
        "Goto next comment",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "comment", Direction::Forward, count)
        },
    );
    catalog.action(
        "goto_prev_comment",
        "Goto previous comment",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "comment", Direction::Backward, count)
        },
    );
    catalog.action(
        "goto_next_test",
        "Goto next test",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "test", Direction::Forward, count)
        },
    );
    catalog.action(
        "goto_prev_test",
        "Goto previous test",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "test", Direction::Backward, count)
        },
    );
    catalog.action(
        "goto_next_entry",
        "Goto next pairing",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "entry", Direction::Forward, count)
        },
    );
    catalog.action(
        "goto_prev_entry",
        "Goto previous pairing",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            mv::goto_ts_object(ed, vid, did, "entry", Direction::Backward, count)
        },
    );

    // Match bracket
    catalog.motion(
        "match_brackets",
        "Goto matching bracket",
        CommandScope::Viewport,
        |_count| Box::new(|ed, vid, did, _m| editing::match_brackets(ed, vid, did)),
    );
}

// ─── Operators ───────────────────────────────────────────────────────

fn register_operators(catalog: &mut impl EngineCommandCatalog) {
    catalog.operator(
        "delete_selection",
        "Delete selection",
        CommandScope::Viewport,
        |ed, vid, did, register| {
            let r = register.unwrap_or_else(|| ed.config().default_yank_register);
            editing::delete_selection(ed, vid, did, r, true)
        },
    );
    catalog.operator(
        "delete_selection_noyank",
        "Delete selection without yanking",
        CommandScope::Viewport,
        |ed, vid, did, register| {
            let r = register.unwrap_or_else(|| ed.config().default_yank_register);
            editing::delete_selection(ed, vid, did, r, false)
        },
    );
    catalog.operator(
        "change_selection",
        "Change selection",
        CommandScope::Viewport,
        |ed, vid, did, register| {
            let r = register.unwrap_or_else(|| ed.config().default_yank_register);
            editing::change_selection(ed, vid, did, r, true);
        },
    );
    catalog.operator(
        "change_selection_noyank",
        "Change selection without yanking",
        CommandScope::Viewport,
        |ed, vid, did, register| {
            let r = register.unwrap_or_else(|| ed.config().default_yank_register);
            editing::change_selection(ed, vid, did, r, false);
        },
    );
    catalog.operator(
        "yank",
        "Yank selection",
        CommandScope::Viewport,
        |ed, vid, did, register| {
            let r = register.unwrap_or_else(|| ed.config().default_yank_register);
            editing::yank(ed, vid, did, r);
        },
    );
    catalog.operator(
        "yank_joined",
        "Join and yank selections",
        CommandScope::Viewport,
        |ed, vid, did, register| {
            let r = register.unwrap_or_else(|| ed.config().default_yank_register);
            editing::yank_joined(ed, vid, did, r, "\n")
        },
    );
}

// ─── Text objects ────────────────────────────────────────────────────

fn register_text_objects(catalog: &mut impl EngineCommandCatalog) {
    // Word text objects
    catalog.text_object(
        "textobject_word",
        "Select inside word",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, obj| {
                editing::textobject_word(ed, vid, did, obj, count, false)
            })
        },
    );
    catalog.text_object(
        "textobject_long_word",
        "Select inside WORD",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, obj| {
                editing::textobject_word(ed, vid, did, obj, count, true)
            })
        },
    );
    catalog.text_object(
        "textobject_paragraph",
        "Select inside paragraph",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, obj| {
                editing::textobject_paragraph(ed, vid, did, obj, count)
            })
        },
    );
    catalog.text_object(
        "textobject_closest_surrounding_pair",
        "Select inside closest surrounding pair (tree-sitter)",
        CommandScope::Viewport,
        |count| {
            Box::new(move |ed, vid, did, obj| {
                editing::textobject_closest_surrounding_pair(ed, vid, did, obj, count)
            })
        },
    );

    // Tree-sitter text objects (registered as Actions for fn-pointer compatibility)
    catalog.action(
        "textobject_function",
        "Select function (tree-sitter)",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            editing::textobject_treesitter(
                ed,
                vid,
                did,
                textobject::TextObject::Around,
                "function.around",
                count,
            )
        },
    );
    catalog.action(
        "textobject_class",
        "Select class/type (tree-sitter)",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            editing::textobject_treesitter(
                ed,
                vid,
                did,
                textobject::TextObject::Around,
                "class.around",
                count,
            )
        },
    );
    catalog.action(
        "textobject_parameter",
        "Select inside argument/parameter (tree-sitter)",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            editing::textobject_treesitter(
                ed,
                vid,
                did,
                textobject::TextObject::Inside,
                "parameter.inside",
                count,
            )
        },
    );
    catalog.action(
        "textobject_comment",
        "Select inside comment (tree-sitter)",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            editing::textobject_treesitter(
                ed,
                vid,
                did,
                textobject::TextObject::Around,
                "comment.around",
                count,
            )
        },
    );
    catalog.action(
        "textobject_test",
        "Select inside test (tree-sitter)",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            editing::textobject_treesitter(
                ed,
                vid,
                did,
                textobject::TextObject::Around,
                "test.around",
                count,
            )
        },
    );
    catalog.action(
        "textobject_entry",
        "Select inside data structure entry (tree-sitter)",
        CommandScope::Viewport,
        |ed, vid, did, count, _| {
            editing::textobject_treesitter(
                ed,
                vid,
                did,
                textobject::TextObject::Around,
                "entry.around",
                count,
            )
        },
    );
}

// ─── Actions ─────────────────────────────────────────────────────────

fn register_actions(catalog: &mut impl EngineCommandCatalog) {
    // Selection manipulation
    catalog.action(
        "collapse_selection",
        "Collapse selection into single cursor",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::collapse_selection(ed, vid, did);
        },
    );
    catalog.action(
        "flip_selections",
        "Flip selection cursor and anchor",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::flip_selections(ed, vid, did);
        },
    );
    catalog.action(
        "ensure_selections_forward",
        "Ensure all selections face forward",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::ensure_selections_forward(ed, vid, did);
        },
    );
    catalog.action(
        "keep_primary_selection",
        "Keep primary selection",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::keep_primary_selection(ed, vid, did);
        },
    );
    catalog.action(
        "remove_primary_selection",
        "Remove primary selection",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::remove_primary_selection(ed, vid, did);
        },
    );
    catalog.action(
        "select_all",
        "Select whole document",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::select_all(ed, vid, did);
        },
    );
    catalog.action(
        "save_selection",
        "Save current selection to jumplist",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::save_selection(ed, vid, did);
        },
    );
    catalog.action(
        "trim_selections",
        "Trim whitespace from selections",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::trim_selections(ed, vid, did);
        },
    );
    catalog.action(
        "align_selections",
        "Align selections in column",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::align_selections(ed, vid, did);
        },
    );
    catalog.action(
        "exit_select_mode",
        "Exit selection mode",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::exit_select_mode(ed, vid, did);
        },
    );
    catalog.action(
        "select_mode",
        "Enter selection extend mode",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::select_mode(ed, vid, did);
        },
    );
    catalog.action(
        "rotate_selections_first",
        "Make the first selection your primary one",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::rotate_selections_first(ed, vid, did);
        },
    );
    catalog.action(
        "rotate_selections_last",
        "Make the last selection your primary one",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::rotate_selections_last(ed, vid, did);
        },
    );

    // Rotation
    catalog.action(
        "rotate_selections_forward",
        "Rotate selections forward",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::rotate_selections_forward(ed, vid, did, count);
        },
    );
    catalog.action(
        "rotate_selections_backward",
        "Rotate selections backward",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::rotate_selections_backward(ed, vid, did, count);
        },
    );
    catalog.action(
        "rotate_selection_contents_forward",
        "Rotate selection contents forward",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::rotate_selection_contents_forward(ed, vid, did, count);
        },
    );
    catalog.action(
        "rotate_selection_contents_backward",
        "Rotate selections contents backward",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::rotate_selection_contents_backward(ed, vid, did, count);
        },
    );
    catalog.action(
        "reverse_selection_contents",
        "Reverse selections contents",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::reverse_selection_contents(ed, vid, did, count);
        },
    );

    // Copy selection on adjacent lines
    catalog.action(
        "copy_selection_on_next_line",
        "Copy selection on next line",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::copy_selection_on_next_line(ed, vid, did, count);
        },
    );
    catalog.action(
        "copy_selection_on_prev_line",
        "Copy selection on previous line",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::copy_selection_on_prev_line(ed, vid, did, count);
        },
    );

    // Line operations
    catalog.action(
        "extend_line",
        "Select current line, if already selected, extend to another line based on the anchor",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            mv::extend_line(ed, vid, did, count);
        },
    );
    catalog.action(
        "extend_line_below",
        "Select current line, if already selected, extend to next line",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            mv::extend_line_below(ed, vid, did, count);
        },
    );
    catalog.action(
        "extend_line_above",
        "Select current line, if already selected, extend to previous line",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            mv::extend_line_above(ed, vid, did, count);
        },
    );
    catalog.action(
        "select_line_below",
        "Select current line, if already selected, extend or shrink line below based on the anchor",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            mv::select_line_below(ed, vid, did, count);
        },
    );
    catalog.action(
        "select_line_above",
        "Select current line, if already selected, extend or shrink line above based on the anchor",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            mv::select_line_above(ed, vid, did, count);
        },
    );
    catalog.action(
        "extend_to_line_bounds",
        "Extend selection to line bounds",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            mv::extend_to_line_bounds(ed, vid, did);
        },
    );
    catalog.action(
        "shrink_to_line_bounds",
        "Shrink selection to line bounds",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            mv::shrink_to_line_bounds(ed, vid, did);
        },
    );
    catalog.action(
        "move_lines_up",
        "Move current line selection up",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::move_lines(ed, vid, did, editing::MoveDirection::Above);
        },
    );
    catalog.action(
        "move_lines_down",
        "Move current line selection down",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::move_lines(ed, vid, did, editing::MoveDirection::Below);
        },
    );
    catalog.action(
        "join_selections",
        "Join lines inside selection",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::join_selections(ed, vid, did);
        },
    );
    catalog.action(
        "join_selections_space",
        "Join lines inside selection and select spaces",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::join_selections_space(ed, vid, did);
        },
    );

    // Paste
    catalog.action(
        "paste_after",
        "Paste after selection",
        CommandScope::Viewport,
        |ed, vid, did, count, reg| {
            let r = reg.unwrap_or_else(|| ed.config().default_yank_register);
            editing::paste_after(ed, vid, did, r, count);
        },
    );
    catalog.action(
        "paste_before",
        "Paste before selection",
        CommandScope::Viewport,
        |ed, vid, did, count, reg| {
            let r = reg.unwrap_or_else(|| ed.config().default_yank_register);
            editing::paste_before(ed, vid, did, r, count);
        },
    );
    catalog.action(
        "paste_clipboard_after",
        "Paste clipboard after selections",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::paste_after(ed, vid, did, '+', count);
        },
    );
    catalog.action(
        "paste_clipboard_before",
        "Paste clipboard before selections",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::paste_before(ed, vid, did, '+', count);
        },
    );
    catalog.action(
        "paste_primary_clipboard_after",
        "Paste primary clipboard after selections",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::paste_after(ed, vid, did, '*', count);
        },
    );
    catalog.action(
        "paste_primary_clipboard_before",
        "Paste primary clipboard before selections",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::paste_before(ed, vid, did, '*', count);
        },
    );
    catalog.action(
        "replace_with_yanked",
        "Replace with yanked text",
        CommandScope::Viewport,
        |ed, vid, did, count, reg| {
            let r = reg.unwrap_or_else(|| ed.config().default_yank_register);
            editing::replace_with_yanked(ed, vid, did, r, count);
        },
    );

    // Indentation
    catalog.action(
        "indent",
        "Indent selection",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::indent(ed, vid, did, count);
        },
    );
    catalog.action(
        "unindent",
        "Unindent selection",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::unindent(ed, vid, did, count);
        },
    );

    // Case
    catalog.action(
        "switch_case",
        "Switch (toggle) case",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::switch_case(ed, vid, did);
        },
    );
    catalog.action(
        "switch_to_uppercase",
        "Switch to uppercase",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::switch_to_uppercase(ed, vid, did);
        },
    );
    catalog.action(
        "switch_to_lowercase",
        "Switch to lowercase",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::switch_to_lowercase(ed, vid, did);
        },
    );

    // Undo/redo
    catalog.action(
        "undo",
        "Undo change",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::undo(ed, vid, did, count);
        },
    );
    catalog.action(
        "redo",
        "Redo change",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::redo(ed, vid, did, count);
        },
    );
    catalog.action(
        "earlier",
        "Move backward in history",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::earlier(ed, vid, did, count);
        },
    );
    catalog.action(
        "later",
        "Move forward in history",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::later(ed, vid, did, count);
        },
    );
    catalog.action(
        "commit_undo_checkpoint",
        "Commit changes to new checkpoint",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::commit_undo_checkpoint(ed, vid, did);
        },
    );

    // Comments
    catalog.action(
        "toggle_comments",
        "Comment/uncomment selections",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::toggle_comments(ed, vid, did);
        },
    );
    catalog.action(
        "toggle_line_comments",
        "Line comment/uncomment selections",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::toggle_line_comments(ed, vid, did);
        },
    );
    catalog.action(
        "toggle_block_comments",
        "Block comment/uncomment selections",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::toggle_block_comments(ed, vid, did);
        },
    );

    // View alignment
    catalog.action(
        "align_view_center",
        "Align view center",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::align_view_center(ed, vid, did);
        },
    );
    catalog.action(
        "align_view_top",
        "Align view top",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::align_view_top(ed, vid, did);
        },
    );
    catalog.action(
        "align_view_bottom",
        "Align view bottom",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::align_view_bottom(ed, vid, did);
        },
    );
    catalog.action(
        "align_view_middle",
        "Align view middle",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::align_view_middle(ed, vid, did);
        },
    );

    // Jumplist
    catalog.action(
        "jump_forward",
        "Jump forward on jumplist",
        CommandScope::Tree,
        |ed, vid, did, count, _reg| {
            editing::jump_forward(ed, vid, did, count);
        },
    );
    catalog.action(
        "jump_backward",
        "Jump backward on jumplist",
        CommandScope::Tree,
        |ed, vid, did, count, _reg| {
            editing::jump_backward(ed, vid, did, count);
        },
    );

    // Newlines
    catalog.action(
        "add_newline_above",
        "Add newline above",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::add_newline_above(ed, vid, did, count);
        },
    );
    catalog.action(
        "add_newline_below",
        "Add newline below",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::add_newline_below(ed, vid, did, count);
        },
    );

    // Scroll
    catalog.action(
        "scroll_up",
        "Scroll view up",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            mv::scroll(ed, vid, did, count, Direction::Backward, false);
        },
    );
    catalog.action(
        "scroll_down",
        "Scroll view down",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            mv::scroll(ed, vid, did, count, Direction::Forward, false);
        },
    );
    catalog.action(
        "page_up",
        "Move page up",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            mv::scroll(ed, vid, did, count, Direction::Backward, true);
        },
    );
    catalog.action(
        "page_down",
        "Move page down",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            mv::scroll(ed, vid, did, count, Direction::Forward, true);
        },
    );
    catalog.action(
        "page_cursor_half_up",
        "Move page and cursor half up",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            mv::scroll(ed, vid, did, count, Direction::Backward, true);
        },
    );
    catalog.action(
        "page_cursor_half_down",
        "Move page and cursor half down",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            mv::scroll(ed, vid, did, count, Direction::Forward, true);
        },
    );

    // Insert mode
    catalog.action(
        "insert_tab",
        "Insert tab char",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::insert_tab(ed, vid, did, count);
        },
    );
    catalog.action(
        "insert_newline",
        "Insert newline char",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::insert_newline(ed, vid, did);
        },
    );
    catalog.action(
        "delete_char_backward",
        "Delete previous char",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::delete_char_backward(ed, vid, did, count);
        },
    );
    catalog.action(
        "delete_char_forward",
        "Delete next char",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::delete_char_forward(ed, vid, did, count);
        },
    );
    catalog.action(
        "delete_word_backward",
        "Delete previous word",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::delete_word_backward(ed, vid, did, count);
        },
    );
    catalog.action(
        "delete_word_forward",
        "Delete next word",
        CommandScope::Viewport,
        |ed, vid, did, count, _reg| {
            editing::delete_word_forward(ed, vid, did, count);
        },
    );
    catalog.action(
        "kill_to_line_start",
        "Delete till start of line",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::kill_to_line_start(ed, vid, did);
        },
    );
    catalog.action(
        "kill_to_line_end",
        "Delete till end of line",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::kill_to_line_end(ed, vid, did);
        },
    );

    // Tree-sitter selection
    catalog.action(
        "expand_selection",
        "Expand selection to parent syntax node",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::expand_selection(ed, vid, did);
        },
    );
    catalog.action(
        "shrink_selection",
        "Shrink selection to previously expanded syntax node",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::shrink_selection(ed, vid, did);
        },
    );
    catalog.action(
        "select_next_sibling",
        "Select next sibling in the syntax tree",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::select_next_sibling(ed, vid, did);
        },
    );
    catalog.action(
        "select_prev_sibling",
        "Select previous sibling the in syntax tree",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::select_prev_sibling(ed, vid, did);
        },
    );
    catalog.action(
        "select_all_siblings",
        "Select all siblings of the current node",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::select_all_siblings(ed, vid, did);
        },
    );
    catalog.action(
        "select_all_children",
        "Select all children of the current node",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::select_all_children(ed, vid, did);
        },
    );
    catalog.action(
        "textobject_change",
        "Select inside VCS change",
        CommandScope::Viewport,
        |ed, vid, did, _count, _reg| {
            editing::textobject_change(ed, vid, did);
        },
    );
    catalog.action(
        "increment",
        "Increment item under cursor",
        CommandScope::Viewport,
        |ed, vid, did, count, reg| {
            let increase_by = if reg == Some('#') { 1 } else { 0 };
            editing::increment(ed, vid, did, count as i64, increase_by);
        },
    );
    catalog.action(
        "decrement",
        "Decrement item under cursor",
        CommandScope::Viewport,
        |ed, vid, did, count, reg| {
            let increase_by = if reg == Some('#') { 1 } else { 0 };
            editing::decrement(ed, vid, did, count as i64, increase_by);
        },
    );
}

// ─── CharPending (wait for next char, then execute) ──────────────────

fn register_char_pending(catalog: &mut impl EngineCommandCatalog) {
    // Find char motions (f/t/F/T)
    catalog.char_pending(
        "find_next_char",
        "Move to next occurrence of char",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, m| {
                mv::find_char(ed, vid, did, ch, Direction::Forward, true, count, m)
            })
        },
    );
    catalog.char_pending(
        "find_till_char",
        "Move till next occurrence of char",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, m| {
                mv::find_char(ed, vid, did, ch, Direction::Forward, false, count, m)
            })
        },
    );
    catalog.char_pending(
        "find_prev_char",
        "Move to previous occurrence of char",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, m| {
                mv::find_char(ed, vid, did, ch, Direction::Backward, true, count, m)
            })
        },
    );
    catalog.char_pending(
        "till_prev_char",
        "Move till previous occurrence of char",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, m| {
                mv::find_char(ed, vid, did, ch, Direction::Backward, false, count, m)
            })
        },
    );

    // Extend variants — always force Movement::Extend
    catalog.char_pending(
        "extend_next_char",
        "Extend to next occurrence of char",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, _m| {
                mv::find_char(
                    ed,
                    vid,
                    did,
                    ch,
                    Direction::Forward,
                    true,
                    count,
                    Movement::Extend,
                )
            })
        },
    );
    catalog.char_pending(
        "extend_till_char",
        "Extend till next occurrence of char",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, _m| {
                mv::find_char(
                    ed,
                    vid,
                    did,
                    ch,
                    Direction::Forward,
                    false,
                    count,
                    Movement::Extend,
                )
            })
        },
    );
    catalog.char_pending(
        "extend_prev_char",
        "Extend to previous occurrence of char",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, _m| {
                mv::find_char(
                    ed,
                    vid,
                    did,
                    ch,
                    Direction::Backward,
                    true,
                    count,
                    Movement::Extend,
                )
            })
        },
    );
    catalog.char_pending(
        "extend_till_prev_char",
        "Extend till previous occurrence of char",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, _m| {
                mv::find_char(
                    ed,
                    vid,
                    did,
                    ch,
                    Direction::Backward,
                    false,
                    count,
                    Movement::Extend,
                )
            })
        },
    );

    // Surround pair text object selection — used as keymap Fallback commands.
    // When the keymap resolves e.g. `mi{char}`, the Fallback mechanism passes
    // the char to the engine which calls resolve(ch, count) → MotionFn.
    catalog.char_pending(
        "select_textobject_inside_surrounding_pair",
        "Select inside surrounding pair",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, _movement| {
                editing::textobject_surrounding_pair(
                    ed,
                    vid,
                    did,
                    textobject::TextObject::Inside,
                    ch,
                    None,
                    count,
                )
            })
        },
    );
    catalog.char_pending(
        "select_textobject_around_surrounding_pair",
        "Select around surrounding pair",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, _movement| {
                editing::textobject_surrounding_pair(
                    ed,
                    vid,
                    did,
                    textobject::TextObject::Around,
                    ch,
                    None,
                    count,
                )
            })
        },
    );
    catalog.char_pending(
        "select_textobject_inside_prev_pair",
        "Select inside previous pair",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, _movement| {
                editing::textobject_surrounding_pair(
                    ed,
                    vid,
                    did,
                    textobject::TextObject::Inside,
                    ch,
                    Some(Direction::Backward),
                    count,
                )
            })
        },
    );
    catalog.char_pending(
        "select_textobject_inside_next_pair",
        "Select inside next pair",
        CommandScope::Viewport,
        |ch, count| {
            Box::new(move |ed, vid, did, _movement| {
                editing::textobject_surrounding_pair(
                    ed,
                    vid,
                    did,
                    textobject::TextObject::Inside,
                    ch,
                    Some(Direction::Forward),
                    count,
                )
            })
        },
    );
}
