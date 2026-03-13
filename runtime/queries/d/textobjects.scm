(function_declaration (function_body) @function.inside) @function.around
(comment) @comment.inside
(comment)+ @comment.around
(class_declaration (aggregate_body) @class.inside) @class.around
(interface_declaration (aggregate_body) @class.inside) @class.around
(struct_declaration (aggregate_body) @class.inside) @class.around
(unittest_declaration (block_statement) @test.inside) @test.around
(parameter) @parameter.inside
(template_parameter) @parameter.inside

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
