use std::sync::Arc;

use super::*;

#[derive(Debug, Default)]
struct Toy {
    pos: usize,
    deleted: usize,
    marks: Vec<String>,
}

fn registry() -> Arc<Registry<Toy>> {
    let mut builder = Builder::new();
    builder.motion_counted(MotionId::new("right"), |count| {
        Box::new(move |toy: &mut Toy, args| {
            assert_eq!(args.count, count);
            toy.pos += args.count;
        })
    });
    builder.operator_with_pending(
        OperatorId::new("delete"),
        "d",
        Some('d'),
        |toy: &mut Toy, _args| {
            toy.deleted += 1;
        },
    );
    builder.action(ActionId::new("mark"), |toy: &mut Toy, args| {
        toy.marks.push(format!("mark:{}", args.count));
    });
    builder.char_pending(CharPendingId::new("find"), |ch, count| {
        CharPendingCommand::Motion(Box::new(move |toy: &mut Toy, args| {
            assert_eq!(args.count, count);
            toy.pos += ch as usize % 10 + args.count;
        }))
    });
    builder.char_pending(CharPendingId::new("replace"), |ch, count| {
        CharPendingCommand::Action(Box::new(move |toy: &mut Toy, args| {
            assert_eq!(args.count, count);
            toy.marks.push(format!("replace:{ch}:{}", args.count));
        }))
    });
    Arc::new(builder.freeze())
}

struct EmptyKeys;

impl KeymapQuery for EmptyKeys {
    fn contains_key(&self, _mode: Mode, _key: Key) -> bool {
        false
    }

    fn pending_is_empty(&self) -> bool {
        true
    }
}

#[test]
fn count_accumulation_applies_to_motion() {
    let registry = registry();
    let mut engine = Helix::new(registry);
    let mut toy = Toy::default();

    assert!(matches!(
        engine.pre_resolve(&mut toy, Mode::Normal, &EmptyKeys, Key::char('1')),
        Some(EngineResult::Pending)
    ));
    assert!(matches!(
        engine.pre_resolve(&mut toy, Mode::Normal, &EmptyKeys, Key::char('2')),
        Some(EngineResult::Pending)
    ));
    assert!(matches!(
        engine.process_lookup(
            &mut toy,
            Mode::Normal,
            Key::char('l'),
            Lookup::Matched(CommandToken::Motion(MotionId::new("right"))),
        ),
        EngineResult::Executed
    ));
    assert_eq!(toy.pos, 12);
}

#[test]
fn operator_pending_flow_executes_motion_then_operator() {
    let registry = registry();
    let mut engine = Vim::new(registry);
    let mut toy = Toy::default();

    assert!(matches!(
        engine.process_lookup(
            &mut toy,
            Mode::Normal,
            Key::char('d'),
            Lookup::Matched(CommandToken::Operator(OperatorId::new("delete"))),
        ),
        EngineResult::Pending
    ));
    assert!(matches!(
        engine.process_lookup(
            &mut toy,
            Mode::Normal,
            Key::char('w'),
            Lookup::Matched(CommandToken::Motion(MotionId::new("right"))),
        ),
        EngineResult::Executed
    ));
    assert_eq!(toy.pos, 1);
    assert_eq!(toy.deleted, 1);
}

#[test]
fn char_pending_motion_and_action_both_execute() {
    let registry = registry();
    let mut engine = Helix::new(registry);
    let mut toy = Toy::default();

    assert!(matches!(
        engine.process_lookup(
            &mut toy,
            Mode::Normal,
            Key::char('f'),
            Lookup::Fallback(CharPendingId::new("find"), 'a'),
        ),
        EngineResult::Executed
    ));
    assert!(toy.pos > 1);

    assert!(matches!(
        engine.process_lookup(
            &mut toy,
            Mode::Normal,
            Key::char('r'),
            Lookup::Fallback(CharPendingId::new("replace"), 'x'),
        ),
        EngineResult::Executed
    ));
    assert_eq!(toy.marks, ["replace:x:1"]);
}

#[test]
fn dot_repeat_replays_last_action_with_count_override() {
    let registry = registry();
    let mut engine = Helix::new(registry);
    let mut toy = Toy::default();

    assert!(matches!(
        engine.process_lookup(
            &mut toy,
            Mode::Normal,
            Key::char('m'),
            Lookup::Matched(CommandToken::Action(ActionId::new("mark"))),
        ),
        EngineResult::Executed
    ));
    assert!(matches!(
        engine.pre_resolve(&mut toy, Mode::Normal, &EmptyKeys, Key::char('3')),
        Some(EngineResult::Pending)
    ));
    assert!(matches!(
        engine.pre_resolve(&mut toy, Mode::Normal, &EmptyKeys, Key::char('.')),
        Some(EngineResult::Executed)
    ));
    assert_eq!(toy.marks, ["mark:1", "mark:3"]);
}

#[test]
fn reset_clears_pending_operator_and_count() {
    let registry = registry();
    let mut engine = Vim::new(registry);
    let mut toy = Toy::default();

    assert!(matches!(
        engine.pre_resolve(&mut toy, Mode::Normal, &EmptyKeys, Key::char('2')),
        Some(EngineResult::Pending)
    ));
    assert!(matches!(
        engine.process_lookup(
            &mut toy,
            Mode::Normal,
            Key::char('d'),
            Lookup::Matched(CommandToken::Operator(OperatorId::new("delete"))),
        ),
        EngineResult::Pending
    ));
    assert!(engine.is_pending());
    engine.reset();
    assert!(!engine.is_pending());
    assert_eq!(engine.input_state(), InputState::default());
    assert_eq!(engine.mode_name(), "NOR");
}
