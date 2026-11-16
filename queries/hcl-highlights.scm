; HCL and Terraform: the crate ships no highlights.

(comment) @comment

(block (identifier) @keyword)
(block (string_lit) @type)
(attribute (identifier) @property)
(object_elem key: (expression (variable_expr (identifier) @property)))
(function_call (identifier) @function)
(get_attr (identifier) @property)
(variable_expr (identifier) @variable)

[
  (string_lit)
  (quoted_template)
  (heredoc_template)
] @string

(template_literal) @string
(heredoc_identifier) @label
(heredoc_start) @label

(template_interpolation_start) @punctuation.special
(template_interpolation_end) @punctuation.special
(template_directive_start) @punctuation.special
(template_directive_end) @punctuation.special

(numeric_lit) @number
(bool_lit) @boolean
(null_lit) @constant

[
  "for"
  "endfor"
  "in"
  "if"
  "else"
  "endif"
] @keyword

[
  "="
  "=>"
  "?"
  ":"
  "!"
  "*"
  "/"
  "%"
  "+"
  "-"
  ">"
  ">="
  "<"
  "<="
  "=="
  "!="
  "&&"
  "||"
] @operator

[
  "."
  ","
] @punctuation.delimiter

[
  (block_start)
  (block_end)
  (object_start)
  (object_end)
  (tuple_start)
  (tuple_end)
  "("
  ")"
  "["
  "]"
] @punctuation.bracket
