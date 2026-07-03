# Pending Registry Entries

Wave 2 implemented the npm, pip, and go entries that were pending in wave 1:

- `pyright` / `basedpyright` - npm
- `typescript-language-server` - npm
- `bash-language-server` - npm
- `yaml-language-server` - npm
- `debugpy` - pip
- `gopls` - go backend

Future registry growth should keep using declarative artifact entries only; do
not add registry-supplied install scripts.
