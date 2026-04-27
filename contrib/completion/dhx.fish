#!/usr/bin/env fish
# Fish completion script for Double Helix editor

complete -c dhx -s h -l help -d "Prints help information"
complete -c dhx -l tutor -d "Loads the tutorial"
complete -c dhx -l health -xa "(__dhx_langs_ops)" -d "Checks for errors"
complete -c dhx -l health -xka all -d "Prints all diagnostic informations"
complete -c dhx -l health -xka all-languages -d "Lists all languages"
complete -c dhx -l health -xka languages -d "Lists user configured languages"
complete -c dhx -l health -xka clipboard -d "Prints system clipboard provider"
complete -c dhx -s g -l grammar -x -a "fetch build" -d "Fetch or build tree-sitter grammars"
complete -c dhx -s v -o vv -o vvv -d "Increases logging verbosity"
complete -c dhx -s V -l version -d "Prints version information"
complete -c dhx -l vsplit -d "Splits all given files vertically"
complete -c dhx -l hsplit -d "Splits all given files horizontally"
complete -c dhx -s c -l config -r -d "Specifies a file to use for config"
complete -c dhx -l log -r -d "Specifies a file to use for logging"
complete -c dhx -s w -l working-dir -d "Specify initial working directory" -xa "(__fish_complete_directories)"

function __dhx_langs_ops
    dhx --health all-languages | tail -n '+2' | string replace -fr '^(\S+) .*' '$1'
end
