use std::sync::Arc;

use helix_modal::core::{
    Builder, CommandToken, Engine, EngineResult, Key, KeymapQuery, Lookup, Mode, MotionArgs,
    MotionId, MotionMode, OperatorArgs, OperatorId, Registry, Vim,
};

#[derive(Debug)]
struct Toy {
    text: Vec<char>,
    cursor: usize,
    anchor: Option<usize>,
}

impl Toy {
    fn new(text: &str) -> Self {
        Self {
            text: text.chars().collect(),
            cursor: 0,
            anchor: None,
        }
    }

    fn move_word(&mut self, args: MotionArgs) {
        if args.kind == MotionMode::Extend && self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
        for _ in 0..args.count {
            while self.cursor < self.text.len() && !self.text[self.cursor].is_whitespace() {
                self.cursor += 1;
            }
            while self.cursor < self.text.len() && self.text[self.cursor].is_whitespace() {
                self.cursor += 1;
            }
        }
    }

    fn delete_selection(&mut self, _args: OperatorArgs) {
        let start = self.anchor.take().unwrap_or(self.cursor).min(self.cursor);
        let end = self.cursor.max(start);
        if start < end {
            self.text.drain(start..end);
            self.cursor = start.min(self.text.len());
        }
    }

    fn as_string(&self) -> String {
        self.text.iter().collect()
    }
}

struct Keys;

impl KeymapQuery for Keys {
    fn contains_key(&self, _mode: Mode, key: Key) -> bool {
        matches!(key.char_value(), Some('w' | 'd'))
    }

    fn pending_is_empty(&self) -> bool {
        true
    }
}

fn registry() -> Arc<Registry<Toy>> {
    let mut builder = Builder::new();
    builder.motion_counted(MotionId::new("word"), |count| {
        Box::new(move |toy: &mut Toy, mut args| {
            args.count = count;
            toy.move_word(args);
        })
    });
    builder.operator_with_pending(
        OperatorId::new("delete"),
        "d",
        Some('d'),
        |toy: &mut Toy, args| {
            toy.delete_selection(args);
        },
    );
    Arc::new(builder.freeze())
}

fn lookup(key: Key) -> Lookup {
    match key.char_value() {
        Some('w') => Lookup::Matched(CommandToken::Motion(MotionId::new("word"))),
        Some('d') => Lookup::Matched(CommandToken::Operator(OperatorId::new("delete"))),
        _ => Lookup::NotFound,
    }
}

fn feed(engine: &mut Vim<Toy>, toy: &mut Toy, key: Key) -> EngineResult {
    if let Some(result) = engine.pre_resolve(toy, Mode::Normal, &Keys, key) {
        return result;
    }
    engine.process_lookup(toy, Mode::Normal, key, lookup(key))
}

fn main() {
    let mut toy = Toy::new("alpha beta gamma delta echo");
    let mut engine = Vim::new(registry());

    for key in ['3', 'w', 'd', 'w', '.'] {
        let result = feed(&mut engine, &mut toy, Key::char(key));
        println!(
            "{key}: {result:?} -> cursor={} text={}",
            toy.cursor,
            toy.as_string()
        );
    }
}
