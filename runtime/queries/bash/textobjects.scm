(function_definition
  body: (_) @function.inside) @function.around

(command
  argument: (_) @parameter.inside)

(comment) @comment.inside

(comment)+ @comment.around

(array
  (_) @entry.around)

; Conditionals

(if_statement) @conditional.around

(case_statement) @conditional.around

; Loops

(for_statement
  body: (_) @loop.inside) @loop.around

(while_statement
  body: (_) @loop.inside) @loop.around

(c_style_for_statement
  body: (_) @loop.inside) @loop.around
