(subroutine_declaration_statement
  body: (_) @function.inside) @function.around
(anonymous_subroutine_expression
  body: (_) @function.inside) @function.around

(package_statement) @class.around
(package_statement
  (block) @class.inside)

(list_expression
  (_) @parameter.inside)

(comment) @comment.around
(pod) @comment.around

; Conditionals

(conditional_statement
  block: (_) @conditional.inside) @conditional.around

; Loops

(loop_statement
  block: (_) @loop.inside) @loop.around

(for_statement
  block: (_) @loop.inside) @loop.around
