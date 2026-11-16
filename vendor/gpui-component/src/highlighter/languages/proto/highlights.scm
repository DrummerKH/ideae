; ideae patch: the crate does not export its highlights.

[
  "syntax"
  "edition"
  "package"
  "option"
  "import"
  "public"
  "weak"
  "service"
  "rpc"
  "returns"
  "stream"
  "message"
  "enum"
  "oneof"
  "map"
  "extend"
  "extensions"
  "optional"
  "required"
  "repeated"
  "reserved"
  "to"
  "max"
] @keyword

[
  (key_type)
  (type)
  (message_or_enum_type)
] @type

[
  (message_name)
  (enum_name)
  (service_name)
] @type

(rpc_name) @function

(field (identifier) @property)
(map_field (identifier) @property)
(oneof_field (identifier) @property)
(oneof (identifier) @property)
(enum_field (identifier) @constant)

(full_ident) @variable

(string) @string
(escape_sequence) @string.escape

[
  (int_lit)
  (float_lit)
  (field_number)
] @number

[
  (true)
  (false)
] @boolean

(comment) @comment

"=" @operator

[
  ";"
  ","
  "."
] @punctuation.delimiter

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
  "<"
  ">"
] @punctuation.bracket
