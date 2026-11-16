; ideae patch: the crate ships no highlights.

(comment) @comment
(description (string_value) @comment.doc)
(string_value) @string

(int_value) @number
(float_value) @number
(boolean_value) @boolean
(null_value) @constant
(enum_value) @constant

(variable) @variable

(named_type (name) @type)
(object_type_definition (name) @type)
(interface_type_definition (name) @type)
(union_type_definition (name) @type)
(enum_type_definition (name) @type)
(input_object_type_definition (name) @type)
(scalar_type_definition (name) @type)
(object_type_extension (name) @type)
(interface_type_extension (name) @type)
(union_type_extension (name) @type)
(enum_type_extension (name) @type)
(input_object_type_extension (name) @type)
(scalar_type_extension (name) @type)

(operation_definition (name) @function)
(fragment_name (name) @function)

(field (name) @property)
(alias (name) @property)
(field_definition (name) @property)
(object_field (name) @property)
(argument (name) @variable)
(input_value_definition (name) @variable)

(directive "@" @attribute (name) @attribute)
(directive_definition "@" @attribute (name) @attribute)
(directive_location) @constant

[
  "query"
  "mutation"
  "subscription"
  "fragment"
  "on"
  "type"
  "interface"
  "union"
  "enum"
  "input"
  "scalar"
  "schema"
  "extend"
  "directive"
  "implements"
  "repeatable"
] @keyword

[
  "="
  "|"
  "&"
  "!"
  "..."
] @operator

":" @punctuation.delimiter
(comma) @punctuation.delimiter

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket
