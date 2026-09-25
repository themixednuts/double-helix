//! The Vim editing engine with its keymap.

use super::*;

fn vim_app() -> AppBuilder {
    let mut config = test_config();
    config.editor.editing_engine = helix_view::editor::EditingEngineConfig::Vim;
    config.keys = helix_term::keymap::vim();
    AppBuilder::new().with_exact_config(config)
}

async fn vim<T: Into<TestCase>>(case: T) -> anyhow::Result<()> {
    test_with_config(vim_app(), case).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operators_take_motions_from_the_cursor() -> anyhow::Result<()> {
    vim(("one #[t|]#wo three\n", "dw", "one #[t|]#hree\n")).await?;
    vim(("one #[t|]#wo three\n", "d$", "one#[ |]#\n")).await?;
    vim(("one tw#[o|]# three\n", "d0", "#[o|]# three\n")).await?;
    vim(("one #[t|]#wo three\n", "2dw", "one#[ |]#\n")).await?;
    // A selection left by an earlier command doesn't widen the operator's range.
    vim(("one #[two|]# three\n", "dw", "one tw#[t|]#hree\n")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn change_word_changes_to_the_end_of_the_word() -> anyhow::Result<()> {
    vim(("#[o|]#ne two\n", "cwxyz<esc>", "xy#[z|]# two\n")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn line_motions_make_operators_linewise() -> anyhow::Result<()> {
    vim(("a\n#[b|]#\nc\nd\n", "dj", "a\n#[d|]#\n")).await?;
    vim(("a\nb\n#[c|]#\nd\n", "dk", "a\n#[d|]#\n")).await?;
    vim(("a\nb\n#[c|]#\nd\n", "dgg", "#[d|]#\n")).await?;
    vim(("a\n#[b|]#\nc\nd\n", "dG", "#[a|]#\n")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doubled_operators_act_on_lines() -> anyhow::Result<()> {
    vim(("a\n#[b|]#\nc\n", "dd", "a\n#[c|]#\n")).await?;
    vim(("a\n#[b|]#\nc\nd\n", "2dd", "a\n#[d|]#\n")).await?;
    vim(("#[a|]#\nb\n", "yyp", "a\n#[a|]#\nb\n")).await?;
    vim(("#[a|]#\n", "<gt><gt>", "\t#[a|]#\n")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_objects_after_i_and_a() -> anyhow::Result<()> {
    vim(("one t#[w|]#o three\n", "diw", "one #[ |]#three\n")).await?;
    vim(("one t#[w|]#o three\n", "daw", "one #[t|]#hree\n")).await?;
    vim(("f(a, #[b|]#)\n", "di(", "f(#[)|]#\n")).await?;
    vim(("f(a, #[b|]#)\n", "da(", "#[f|]#\n")).await?;
    vim(("x = \"a #[b|]# c\"\n", "ci\"z<esc>", "x = \"#[z|]#\"\n")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dot_repeats_operator_changes() -> anyhow::Result<()> {
    vim(("#[a|]# b c d\n", "dw.", "#[c|]# d\n")).await?;
    vim(("a\n#[b|]#\nc\nd\n", "dd.", "a\n#[d|]#\n")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_key_edits() -> anyhow::Result<()> {
    vim(("#[a|]#bc\n", "x", "#[b|]#c\n")).await?;
    vim(("#[a|]#bc\n", "2x", "#[c|]#\n")).await?;
    vim(("a#[b|]#c\n", "D", "#[a|]#\n")).await?;
    vim(("a#[b|]#c\n", "Cz<esc>", "a#[z|]#\n")).await?;
    vim(("#[a|]#bc\n", "~", "A#[b|]#c\n")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn visual_modes() -> anyhow::Result<()> {
    // Characterwise, then delete.
    vim(("#[a|]#bcd\n", "vld", "#[c|]#d\n")).await?;
    // Linewise: the selection covers whole lines as the cursor moves.
    vim(("a\n#[b|]#\nc\nd\n", "Vjd", "a\n#[d|]#\n")).await?;
    // Blockwise: a column across lines.
    vim(("#[a|]#b\ncd\nef\n", "<C-v>jjd", "#[b|]#\nd\nf\n")).await?;
    // `Esc` leaves visual mode with the cursor where it was.
    vim(("#[a|]#bc\n", "vl<esc>", "a#[b|]#c\n")).await?;
    // Text objects select in visual mode.
    vim(("one t#[w|]#o three\n", "viwd", "one #[ |]#three\n")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insert_mode_arrows_move() -> anyhow::Result<()> {
    vim(("#[a|]#bc\n", "i<right><right>x<esc>", "ab#[x|]#c\n")).await?;
    Ok(())
}
