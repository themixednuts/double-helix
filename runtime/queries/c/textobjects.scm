(function_definition
  body: (_) @function.inside) @function.around

(struct_specifier
  body: (_) @class.inside) @class.around

(enum_specifier
  body: (_) @class.inside) @class.around

(union_specifier
  body: (_) @class.inside) @class.around

(parameter_list 
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(argument_list
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(comment) @comment.inside

(comment)+ @comment.around

(enumerator
  (_) @entry.inside) @entry.around

(initializer_list
  (_) @entry.around)

; Conditionals

(if_statement
  consequence: (_) @conditional.inside) @conditional.around

(switch_statement
  body: (_) @conditional.inside) @conditional.around

; Loops

(for_statement
  body: (_) @loop.inside) @loop.around

(while_statement
  body: (_) @loop.inside) @loop.around

(do_statement
  body: (_) @loop.inside) @loop.around
