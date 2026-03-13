[
  (class_declaration body: (_) @class.inside)
  (struct_declaration body: (_) @class.inside)
  (interface_declaration body: (_) @class.inside)
  (enum_declaration body: (_) @class.inside)
  (delegate_declaration)
  (record_declaration body: (_) @class.inside)
] @class.around

(constructor_declaration body: (_) @function.inside) @function.around

(destructor_declaration body: (_) @function.inside) @function.around

(method_declaration body: (_) @function.inside) @function.around

(property_declaration (_) @function.inside) @function.around

(parameter (_) @parameter.inside) @parameter.around

(comment)+ @comment.around

; Conditionals

(if_statement
  consequence: (_) @conditional.inside) @conditional.around

(switch_statement
  body: (_) @conditional.inside) @conditional.around

; Loops

(for_statement
  body: (_) @loop.inside) @loop.around

(foreach_statement
  body: (_) @loop.inside) @loop.around

(while_statement
  body: (_) @loop.inside) @loop.around

(do_statement
  body: (_) @loop.inside) @loop.around
