(function_declaration
  body: (_) @function.inside) @function.around

(function_expression
  body: (_) @function.inside) @function.around

(arrow_function
  body: (_) @function.inside) @function.around

(method_definition
  body: (_) @function.inside) @function.around

(generator_function_declaration
  body: (_) @function.inside) @function.around

(class_declaration
  body: (class_body) @class.inside) @class.around

(class
  (class_body) @class.inside) @class.around

(export_statement
  declaration: [
    (function_declaration) @function.around
    (class_declaration) @class.around 
  ])

(formal_parameters
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(arguments
  ((_) @parameter.inside . ","? @parameter.around) @parameter.around)

(comment) @comment.inside

(comment)+ @comment.around

(array 
  (_) @entry.around)

(pair
  (_) @entry.inside) @entry.around

(pair_pattern
  (_) @entry.inside) @entry.around

; Conditionals

(if_statement
  consequence: (_) @conditional.inside) @conditional.around

(switch_statement
  body: (_) @conditional.inside) @conditional.around

(ternary_expression
  consequence: (_) @conditional.inside) @conditional.around

; Loops

(for_statement
  body: (_) @loop.inside) @loop.around

(for_in_statement
  body: (_) @loop.inside) @loop.around

(while_statement
  body: (_) @loop.inside) @loop.around

(do_statement
  body: (_) @loop.inside) @loop.around
