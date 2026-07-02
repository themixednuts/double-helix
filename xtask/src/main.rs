mod docgen;
mod fff_upstream;
mod helpers;
mod path;

use std::{env, error::Error};

type DynError = Box<dyn Error>;

pub mod tasks {
    use crate::DynError;
    use std::collections::HashSet;

    pub fn docgen() -> Result<(), DynError> {
        use crate::docgen::*;
        write(TYPABLE_COMMANDS_MD_OUTPUT, &typable_commands()?);
        write(STATIC_COMMANDS_MD_OUTPUT, &static_commands()?);
        write(LANG_SUPPORT_MD_OUTPUT, &lang_features()?);
        Ok(())
    }

    pub fn querycheck(languages: impl Iterator<Item = String>) -> Result<(), DynError> {
        use helix_core::syntax::LanguageData;

        let languages_to_check: HashSet<_> = languages.collect();
        let loader = helix_core::config::default_lang_loader();
        for (_language, lang_data) in loader.languages() {
            if !languages_to_check.is_empty()
                && !languages_to_check.contains(&lang_data.config().language_id)
            {
                continue;
            }
            let config = lang_data.config();
            let Some(syntax_config) = LanguageData::compile_syntax_config(config, &loader)? else {
                continue;
            };
            let grammar = syntax_config.grammar;
            LanguageData::compile_indent_query(grammar, config)?;
            LanguageData::compile_textobject_query(grammar, config)?;
            LanguageData::compile_tag_query(grammar, config)?;
            LanguageData::compile_rainbow_query(grammar, config)?;
        }

        println!("Query check succeeded");

        Ok(())
    }

    pub fn themecheck(themes: impl Iterator<Item = String>) -> Result<(), DynError> {
        use helix_view::theme::Loader;

        let themes_to_check: HashSet<_> = themes.collect();

        let theme_names = [
            vec!["default".to_string(), "base16_default".to_string()],
            Loader::read_names(&crate::path::themes()),
        ]
        .concat();
        let loader = Loader::new(&[crate::path::runtime()]);
        let mut errors_present = false;

        for name in theme_names {
            if !themes_to_check.is_empty() && !themes_to_check.contains(&name) {
                continue;
            }

            let (_, warnings) = loader.load_with_warnings(&name).unwrap();

            if !warnings.is_empty() {
                errors_present = true;
                println!("Theme '{name}' loaded with errors:");
                for warning in warnings {
                    println!("\t* {}", warning);
                }
            }
        }

        match errors_present {
            true => Err("Errors found when loading bundled themes".into()),
            false => {
                println!("Theme check successful!");
                Ok(())
            }
        }
    }

    pub fn arch_check() -> Result<(), DynError> {
        use std::process::Command;

        let workspace_root = crate::path::project_root();

        // helix-core must not depend on helix-term, helix-tui, or crossterm
        let core_tree = Command::new("cargo")
            .args(["tree", "-p", "helix-core", "--format", "{p}"])
            .current_dir(&workspace_root)
            .output()?;
        if !core_tree.status.success() {
            return Err(format!(
                "cargo tree -p helix-core failed: {}",
                String::from_utf8_lossy(&core_tree.stderr)
            )
            .into());
        }
        let core_deps = String::from_utf8_lossy(&core_tree.stdout);
        for forbidden in ["helix-term", "helix-tui", "crossterm"] {
            if core_deps.contains(forbidden) {
                return Err(format!(
                    "helix-core must not depend on {} (layering violation)",
                    forbidden
                )
                .into());
            }
        }

        // helix-view without term feature must not depend on termina or crossterm
        // --edges no-dev excludes dev-deps (e.g. helix-tui) that pull in terminal crates
        let view_tree = Command::new("cargo")
            .args([
                "tree",
                "-p",
                "helix-view",
                "--no-default-features",
                "--edges",
                "no-dev",
                "--format",
                "{p}",
            ])
            .current_dir(&workspace_root)
            .output()?;
        if !view_tree.status.success() {
            return Err(format!(
                "cargo tree -p helix-view --no-default-features failed: {}",
                String::from_utf8_lossy(&view_tree.stderr)
            )
            .into());
        }
        let view_deps = String::from_utf8_lossy(&view_tree.stdout);
        for forbidden in ["termina", "crossterm"] {
            if view_deps.contains(forbidden) {
                return Err(format!(
                    "helix-view (without term feature) must not depend on {} (layering violation)",
                    forbidden
                )
                .into());
            }
        }

        println!("Architectural checks passed.");
        Ok(())
    }

    pub fn fff_upstream(args: impl Iterator<Item = String>) -> Result<(), DynError> {
        crate::fff_upstream::check(args)
    }

    pub fn print_help() {
        println!(
            "
Usage: Run with `cargo xtask <task>`, eg. `cargo xtask docgen`.

    Tasks:
        docgen                     Generate files to be included in the mdbook output.
        query-check [languages]    Check that tree-sitter queries are valid for the given
                                   languages, or all languages if none are specified.
        theme-check [themes]       Check that the theme files in runtime/themes/ are valid for the
                                   given themes, or all themes if none are specified.
        arch-check                 Verify layering: helix-core has no frontend deps; helix-view
                                   has no terminal deps when built without term feature.
        fff-upstream [--ref REF]   Compare vendored FFF core against upstream fff.nvim.
                     [--fail-on-drift]
"
        );
    }
}

fn main() -> Result<(), DynError> {
    let mut args = env::args().skip(1);
    let task = args.next();
    match task {
        None => tasks::print_help(),
        Some(t) => match t.as_str() {
            "docgen" => tasks::docgen()?,
            "query-check" => tasks::querycheck(args)?,
            "theme-check" => tasks::themecheck(args)?,
            "arch-check" => tasks::arch_check()?,
            "fff-upstream" => tasks::fff_upstream(args)?,
            invalid => return Err(format!("Invalid task name: {}", invalid).into()),
        },
    };
    Ok(())
}
