# Vim engine

`editing-engine = "vim"` in `[editor]` switches to Vim-style editing: an operator comes first
and waits for the text it acts on.

```toml
[editor]
editing-engine = "vim"
```

The engine brings its own keymap. Your `[keys]` config applies on top of it, as it does on top
of the Helix keymap.

## Operators

`d` delete, `c` change, `y` yank, `>` indent, `<` unindent, `gu` lowercase, `gU` uppercase and
`g~` toggle case each wait for a motion or a text object:

- `dw`, `c$`, `y3e`, `>}`: from the cursor to where the motion goes.
- `dj`, `yG`, `>gg`, `dL`: motions between lines (`j`, `k`, `gg`, `G`, `H`, `M`, `L`) act on
  whole lines.
- `dd`, `yy`, `cc`, `>>`, `guu`: the operator doubled acts on the current line (with a count,
  that many lines).
- `dfx`, `ct)`: find and till are inclusive and exclusive, as in Vim.
- `cw` changes to the end of the word, as in Vim.

Counts multiply: `2d3w` deletes six words. `"a` before an operator picks register `a`. `.`
repeats the last change, with a new count if you give one.

## Text objects

After an operator, or in visual mode, `i` (inside) or `a` (around) followed by:

| Key | Object |
| --- | --- |
| `w`, `W` | word, WORD |
| `p` | paragraph |
| `(` `)` `b`, `{` `}` `B`, `[` `]`, `<` `>` | bracket pair |
| `"`, `'`, `` ` `` | quotes |
| `m` | the nearest enclosing pair of any kind |
| `f`, `c`, `a`, `/` | function, class, argument, comment (tree-sitter) |

## Visual modes

`v` selects characters, `V` whole lines and `C-v` a block of columns (one selection per line,
so edits apply to every line of the block). Motions move the selection's end. `o` swaps its
ends. `d`/`x`, `c`/`s`, `y`, `>`, `<`, `~`, `u`, `U`, `J` and `p` act on the selection and
return to normal mode. `I` and `A` insert before or after it. `v`, `V` and `C-v` switch between
the modes, or leave the one you are in. `Esc` leaves visual mode.

## Other keys

`x` `X` delete characters, `D` `C` `Y` act to the end of the line (`Y` yanks whole lines), `s`
and `S` substitute characters and lines, `r` replaces characters, `~` toggles case and moves on,
`J` joins lines, `p` `P` paste, `u` and `C-r` undo and redo, `q` and `@` record and replay
macros, and `;` repeats the last find.

The Helix menus stay where they are: `space` (pickers, LSP, the clipboard), `C-w` (windows), `z`
(view), `[` and `]` (goto previous and next), and `m` (match and surround).
