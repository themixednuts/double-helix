//! The keymap for `editing-engine = "vim"`.
//!
//! Keys follow Vim. Operators (`d`, `c`, `y`, `>`, `<`, `gu`, `gU`, `g~`) wait for a motion or
//! text object in the engine; the same keys act on the selection in visual mode. Menus Vim has
//! no use for of its own (`space`, `C-w`, `z`, `[`, `]`, `m`) come from the Helix keymap.

use std::collections::HashMap;

use super::macros::keymap;
use super::{KeyTrie, KeyTrieNode, Mode};
use helix_core::hashmap;
use helix_view::input::KeyEvent;

/// Keys whose Helix menus the Vim keymap shares.
const SHARED_MENUS: &[&str] = &["space", "C-w", "z", "Z", "[", "]", "m"];

pub fn vim() -> HashMap<Mode, KeyTrie> {
    let helix = super::default::default();

    let mut normal = keymap!({ "Normal mode"
        "h" | "left" | "backspace" => move_char_left,
        "j" | "down" => move_line_down,
        "k" | "up" => move_line_up,
        "l" | "right" => move_char_right,

        "w" => move_next_word_start,
        "b" => move_prev_word_start,
        "e" => move_next_word_end,
        "W" => move_next_long_word_start,
        "B" => move_prev_long_word_start,
        "E" => move_next_long_word_end,

        "0" | "home" => goto_line_start,
        "^" => goto_first_nonwhitespace,
        "$" | "end" => goto_line_end,
        "G" => vim_goto_line,
        "H" => goto_window_top,
        "M" => goto_window_center,
        "L" => goto_window_bottom,
        "%" => match_brackets,
        "{" => goto_prev_paragraph,
        "}" => goto_next_paragraph,

        "f" => { "Find char" fallback=find_next_char },
        "t" => { "Till char" fallback=find_till_char },
        "F" => { "Find char backward" fallback=find_prev_char },
        "T" => { "Till char backward" fallback=till_prev_char },
        ";" | "A-." => repeat_last_motion,

        "g" => { "Goto"
            "g" => goto_file_start,
            "e" => move_prev_word_end,
            "E" => move_prev_long_word_end,
            "j" => move_visual_line_down,
            "k" => move_visual_line_up,
            "_" => goto_line_end,
            "d" => goto_definition,
            "D" => goto_declaration,
            "y" => goto_type_definition,
            "r" => goto_reference,
            "i" => goto_implementation,
            "t" => goto_next_buffer,
            "T" => goto_previous_buffer,
            "a" => goto_last_accessed_file,
            "m" => goto_last_modified_file,
            "." => goto_last_modification,
            "w" => goto_word,
            "f" => goto_file,
            "u" => vim_lowercase,
            "U" => vim_uppercase,
            "~" => vim_toggle_case,
            "J" => join_selections_space,
        },

        "i" => insert_mode,
        "I" => insert_at_line_start,
        "a" => append_mode,
        "A" => insert_at_line_end,
        "o" => open_below,
        "O" => open_above,

        "v" => vim_visual_char,
        "V" => vim_visual_line,
        "C-v" => vim_visual_block,

        "d" => delete_selection,
        "c" => change_selection,
        "y" => yank,
        ">" => vim_indent,
        "<" => vim_unindent,

        "x" | "del" => vim_delete_char,
        "X" => vim_delete_char_backward,
        "D" => vim_delete_to_line_end,
        "C" => vim_change_to_line_end,
        "Y" => vim_yank_line,
        "s" => vim_substitute,
        "S" => vim_substitute_line,
        "r" => replace,
        "~" => vim_toggle_case_char,
        "J" => join_selections,
        "p" => paste_after,
        "P" => paste_before,

        "u" => undo,
        "C-r" => redo,

        "/" => search,
        "?" => rsearch,
        "n" => search_next,
        "N" => search_prev,
        "*" => search_selection_detect_word_boundaries,

        ":" => command_mode,
        "q" => record_macro,
        "@" => replay_macro,

        "C-d" => page_cursor_half_down,
        "C-u" => page_cursor_half_up,
        "C-f" | "pagedown" => page_down,
        "C-b" | "pageup" => page_up,
        "C-e" => scroll_down,
        "C-y" => scroll_up,
        "C-o" => jump_backward,
        "C-i" | "tab" => jump_forward,
        "C-a" => increment,
        "C-x" => decrement,
        "K" => hover,
        "esc" => normal_mode,
    });
    share_menus(&mut normal, &helix[&Mode::Normal]);

    let mut select = normal.clone();
    select.merge_nodes(keymap!({ "Visual mode"
        "d" | "x" | "X" | "D" => delete_selection,
        "c" | "s" | "S" | "C" | "R" => change_selection,
        "y" | "Y" => yank,
        ">" => vim_indent,
        "<" => vim_unindent,
        "~" => vim_toggle_case,
        "u" => vim_lowercase,
        "U" => vim_uppercase,
        "J" => join_selections,
        "p" | "P" => replace_with_yanked,
        "o" | "O" => flip_selections,
        "I" => insert_mode,
        "A" => append_mode,
    }));

    hashmap!(
        Mode::Normal => normal,
        Mode::Select => select,
        Mode::Insert => helix[&Mode::Insert].clone(),
    )
}

/// Copy the Helix keymap's menus the Vim keymap reuses into `normal`.
fn share_menus(normal: &mut KeyTrie, helix_normal: &KeyTrie) {
    let mut map = HashMap::new();
    let mut order = Vec::new();
    for key in SHARED_MENUS {
        let key: KeyEvent = key.parse().expect("valid key");
        if let Some(menu) = helix_normal.search(&[key]) {
            map.insert(key, menu.clone());
            order.push(key);
        }
    }
    normal.merge_nodes(KeyTrie::Node(KeyTrieNode::new("", map, order)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vim_keymap_binds_vim_keys() {
        let keymap = vim();
        let normal = &keymap[&Mode::Normal];
        let command = |keys: &str| {
            let keys: Vec<KeyEvent> = helix_view::input::parse_macro(keys).unwrap();
            match normal.search(&keys) {
                Some(KeyTrie::MappableCommand(command)) => command.name().to_string(),
                other => panic!("{keys:?} is not a command: {other:?}"),
            }
        };
        assert_eq!(command("x"), "vim_delete_char");
        assert_eq!(command("$"), "goto_line_end");
        assert_eq!(command("0"), "goto_line_start");
        assert_eq!(command("V"), "vim_visual_line");
        assert_eq!(command("gu"), "vim_lowercase");
        // Shared Helix menus.
        assert_eq!(command("<space>f"), "file_picker");
        assert_eq!(command("<C-w>v"), "vsplit");
        assert_eq!(command("zz"), "align_view_center");

        let select = &keymap[&Mode::Select];
        match select.search(&["x".parse().unwrap()]) {
            Some(KeyTrie::MappableCommand(command)) => {
                assert_eq!(command.name(), "delete_selection")
            }
            other => panic!("visual x: {other:?}"),
        }
    }
}
