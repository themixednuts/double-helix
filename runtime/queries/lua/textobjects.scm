(function_definition
  body: (_) @function.inside) @function.around

(function_declaration
  body: (_) @function.inside) @function.around

(parameters
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(arguments
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(comment) @comment.inside

(comment)+ @comment.around

(table_constructor
  (field (_) @entry.inside) @entry.around)

; Conditionals

(if_statement
  consequence: (_) @conditional.inside) @conditional.around

; Loops

(for_statement
  body: (_) @loop.inside) @loop.around

(while_statement
  body: (_) @loop.inside) @loop.around

(repeat_statement
  body: (_) @loop.inside) @loop.around
