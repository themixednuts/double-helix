(class_declaration
  body: (_) @class.inside) @class.around

(protocol_declaration
  body: (_) @class.inside) @class.around

(function_declaration
  body: (_) @function.inside) @function.around

(init_declaration
  body: (_) @function.inside) @function.around

(deinit_declaration
  body: (_) @function.inside) @function.around

(subscript_declaration
  (computed_property) @function.inside) @function.around

; Closures: the body lives in a `statements` child (the `{ … in }` signature is
; separate), so `*.inside` is the body alone; optional so empty closures match.
(lambda_literal
  (statements)? @function.inside) @function.around

(parameter
  (_) @parameter.inside) @parameter.around

(lambda_parameter
  (_) @parameter.inside) @parameter.around

(value_argument
  (_) @parameter.inside) @parameter.around

[
  (comment)
  (multiline_comment)
] @comment.inside

(comment)+ @comment.around

(multiline_comment) @comment.around

; Conditionals

(if_statement
  (statements) @conditional.inside) @conditional.around

(guard_statement
  (statements) @conditional.inside) @conditional.around

(switch_statement) @conditional.around

; Loops

(for_statement
  (statements) @loop.inside) @loop.around

(while_statement
  (statements) @loop.inside) @loop.around

(repeat_while_statement
  (statements) @loop.inside) @loop.around
