(method_declaration
  body: (_)? @function.inside) @function.around

(constructor_declaration
  body: (_) @function.inside) @function.around

(interface_declaration
  body: (_) @class.inside) @class.around

(class_declaration
  body: (_) @class.inside) @class.around

(record_declaration
  body: (_) @class.inside) @class.around

(enum_declaration
  body: (_) @class.inside) @class.around

(formal_parameters
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(type_parameters
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(type_arguments
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(argument_list
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

[
  (line_comment)
  (block_comment)
] @comment.inside

(line_comment)+ @comment.around

(block_comment) @comment.around

(array_initializer
  (_) @entry.around)

(enum_body
  (enum_constant) @entry.around)

; Conditionals

(if_statement
  consequence: (_) @conditional.inside) @conditional.around

(switch_expression
  body: (_) @conditional.inside) @conditional.around

; Loops

(for_statement
  body: (_) @loop.inside) @loop.around

(enhanced_for_statement
  body: (_) @loop.inside) @loop.around

(while_statement
  body: (_) @loop.inside) @loop.around

(do_statement
  body: (_) @loop.inside) @loop.around
