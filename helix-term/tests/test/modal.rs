//! Modal engine behavior shared by keymaps: char-pending commands, repeats, text objects.

use super::*;

use helix_core::hashmap;
use helix_term::keymap;
use helix_view::document::Mode;

/// The Helix keymap with `extra` bound in normal mode.
fn with_normal_keys(extra: keymap::KeyTrie) -> AppBuilder {
    let mut config = test_config();
    keymap::merge_keys(
        &mut config.keys,
        std::collections::HashMap::from([(Mode::Normal, extra)]),
    );
    AppBuilder::new().with_exact_config(config)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeat_last_motion_repeats_find() -> anyhow::Result<()> {
    // As pressing `fc` again: from the found `c` through the next one.
    test(("#[a|]#bcabcabc\n", "fc<A-.>", "ab#[cabc|]#abc\n")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn char_pending_command_bound_to_a_key_waits_for_the_char() -> anyhow::Result<()> {
    let app = || {
        with_normal_keys(helix_term::keymap!({ "Normal mode"
            "C-f" => find_next_char,
        }))
    };
    test_with_config(app(), ("#[a|]#bcd\n", "<C-f>c", "#[abc|]#d\n")).await?;
    // Esc gives up waiting.
    test_with_config(app(), ("#[a|]#bcd\n", "<C-f><esc>l", "a#[b|]#cd\n")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_object_command_bound_to_a_key_selects_inside() -> anyhow::Result<()> {
    let app = || {
        with_normal_keys(helix_term::keymap!({ "Normal mode"
            "C-t" => textobject_word,
            "C-y" => textobject_word_around,
        }))
    };
    test_with_config(
        app(),
        ("one t#[w|]#o three\n", "<C-t>", "one #[two|]# three\n"),
    )
    .await?;
    test_with_config(
        app(),
        ("one t#[w|]#o three\n", "<C-y>", "one #[two |]#three\n"),
    )
    .await?;
    Ok(())
}
