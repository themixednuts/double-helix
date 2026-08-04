# Understand MVP

`:understand` (alias `:code-atlas`) opens a generated briefing in a normal split
beside the source. It is an index into the code, not a chat transcript or a graph
viewer: every actionable line cites source, and moving through the briefing previews
that evidence in the source split.

The first offline slice briefs the current file using its tree-sitter syntax tree.
It groups supported definitions under **Types**, **Starts here**, and **Does the
work**, then reports mechanically detected boundaries and explicit unresolved areas.
No model or network connection is required.

## Controls

- `j` / `k` or arrows: move between cited claims
- `tab`: unfold or fold mechanical detail
- `enter`: zoom from a file claim into its symbol briefing
- `-`: return to the previous briefing scope
- `o` or `esc`: focus the source while keeping the briefing open
- `q`: close the briefing and restore the original source position

The generated document remains a real editor buffer, so native split navigation,
themes, scrolling, and source editing still work. A thin session component only
keeps the briefing read-only and synchronizes its cursor with the source view. After
a source edit, returning to the briefing refreshes it at file scope once the syntax
tree is current, so stored citations are never silently reused against changed text.

## Honest output

The interface distinguishes three things:

- grounded claims, each constructed with at least one source citation
- analysis coverage, which reports available tools instead of an “understood” score
- unresolved behavior, such as dynamic dispatch, macro expansion, or unsupported
  syntax structure

Calls and mentions are labeled as syntactic or textual. The offline adapter does not
claim runtime behavior, complete call graphs, or semantic reference resolution.

## Composable boundary

`code-atlas` is editor- and protocol-agnostic. Its central primitives are
`Subject`, `Briefing<T>`, `Section<T>`, `Claim<T>`, `Evidence<T>`, and
`Unresolved<T>`. The adapter owns `T`; Helix uses a document-and-range anchor, while
another editor can use an LSP location or its own buffer handle.

AI is progressive enhancement. A future provider may rewrite a mechanical lead or
add explicitly inferred claims, but it must retain citations and tiers. Turning AI
off removes enrichment without changing the interface, navigation, or offline
briefing.
